//! Geometric clipping of one painted screen to the animated iris.
use egui::epaint::{Mesh, Primitive, Vertex};
use egui::{Color32, LayerId, Pos2, Rect, Shape, Vec2};

pub(super) fn start(ctx: &egui::Context, layer: LayerId) -> egui::layers::ShapeIdx {
    ctx.graphics_mut(|g| g.entry(layer).next_idx())
}

pub(super) fn finish(ctx: &egui::Context, layer: LayerId, start: egui::layers::ShapeIdx, clip: Rect, radius: f32) {
    let circle = Circle::new(clip.center(), radius, 1.0 / ctx.pixels_per_point());
    finish_mask(ctx, layer, start, clip, &circle);
}

pub(super) fn finish_rounded(ctx: &egui::Context, layer: LayerId, start: egui::layers::ShapeIdx, clip: Rect, rect: Rect, radius: f32) {
    let mask = Circle::rounded(rect, radius, 1.0 / ctx.pixels_per_point());
    finish_mask(ctx, layer, start, clip, &mask);
}

fn finish_mask(ctx: &egui::Context, layer: LayerId, start: egui::layers::ShapeIdx, clip: Rect, mask: &Circle) {
    let shapes = ctx.graphics_mut(|graphics| {
        let list = graphics.entry(layer);
        let end = list.next_idx().0;
        let shapes = list.all_entries().skip(start.0).cloned().collect::<Vec<_>>();
        for index in start.0..end {
            list.reset_shape(egui::layers::ShapeIdx(index));
        }
        shapes
    });
    let scale = ctx.pixels_per_point();
    let painter = ctx.layer_painter(layer);
    for primitive in ctx.tessellate(shapes, scale) {
        let clip_rect = primitive.clip_rect.intersect(clip);
        if !clip_rect.is_positive() { continue; }
        // Rectangular paint callbacks cannot honor a geometric mask. The normal
        // page renders them directly when the transition has settled.
        let Primitive::Mesh(mesh) = primitive.primitive else { continue };
        let mesh = mask.clip_mesh(mesh, clip_rect);
        if !mesh.indices.is_empty() {
            painter.with_clip_rect(clip_rect).add(Shape::mesh(mesh));
        }
    }
}

struct Circle {
    center: Pos2,
    inner: f32,
    outer: f32,
    inset_sq: f32,
    core: Vec<Pos2>,
    rim: Vec<Rim>,
}

struct Rim {
    points: [Pos2; 4],
    bounds: Rect,
    normal: Vec2,
}

impl Circle {
    fn new(center: Pos2, radius: f32, feather: f32) -> Self {
        let inner = (radius - feather * 0.5).max(0.0);
        let outer = radius + feather * 0.5;
        // Keep chord error subpixel at large window sizes.
        let sides = ((std::f32::consts::PI * (outer / (feather * 0.4)).sqrt()).ceil() as usize).clamp(32, 192);
        let angle = std::f32::consts::TAU / sides as f32;
        let positions = |r: f32| (0..sides).map(|i| {
            let (s, c) = (angle * i as f32).sin_cos();
            center + egui::vec2(c, s) * r
        }).collect::<Vec<_>>();
        let core = positions(inner);
        let edge = positions(outer);
        let rim = (0..sides).map(|i| {
            let j = (i + 1) % sides;
            let points = [core[i], edge[i], edge[j], core[j]];
            let bounds = points.iter().fold(Rect::NOTHING, |mut r, &p| { r.extend_with(p); r });
            let (s, c) = (angle * (i as f32 + 0.5)).sin_cos();
            Rim { points, bounds, normal: egui::vec2(c, s) }
        }).collect();
        Self { center, inner, outer, inset_sq: (inner * (angle * 0.5).cos()).powi(2), core, rim }
    }

    fn rounded(rect: Rect, radius: f32, feather: f32) -> Self {
        let half = feather * 0.5;
        let radius = radius.clamp(half + 0.001, (rect.size().min_elem() * 0.5).max(half + 0.001));
        let steps = (((radius + half) / feather).sqrt().ceil() as usize * 2).clamp(5, 36);
        let positions = |rect: Rect, radius: f32| {
            let centers = [
                rect.right_top() + egui::vec2(-radius, radius),
                rect.right_bottom() + egui::vec2(-radius, -radius),
                rect.left_bottom() + egui::vec2(radius, -radius),
                rect.left_top() + egui::vec2(radius, radius),
            ];
            let mut points = Vec::with_capacity(4 * (steps + 1));
            for (i, center) in centers.into_iter().enumerate() {
                for j in 0..=steps {
                    let angle = (i as f32 - 1.0 + j as f32 / steps as f32) * std::f32::consts::FRAC_PI_2;
                    let (s, c) = angle.sin_cos();
                    points.push(center + egui::vec2(c, s) * radius);
                }
            }
            points
        };
        let inside = rect.shrink(half);
        let outside = rect.expand(half);
        let core = positions(inside, radius - half);
        let edge = positions(outside, radius + half);
        let rim = (0..edge.len()).filter_map(|i| {
            let j = (i + 1) % edge.len();
            let side = edge[j] - edge[i];
            if side.length_sq() <= f32::EPSILON { return None; }
            let points = [core[i], edge[i], edge[j], core[j]];
            let bounds = points.iter().fold(Rect::NOTHING, |mut r, &p| { r.extend_with(p); r });
            let normal = egui::vec2(side.y, -side.x).normalized();
            Some(Rim { points, bounds, normal })
        }).collect();
        Self {
            center: rect.center(), inner: inside.size().min_elem() * 0.5,
            outer: outside.size().length() * 0.5,
            inset_sq: (inside.size().min_elem() * 0.5 * (std::f32::consts::PI / 20.0).cos()).powi(2),
            core, rim,
        }
    }

    fn clip_mesh(&self, mesh: Mesh, visible: Rect) -> Mesh {
        let mut output = Mesh::with_texture(mesh.texture_id);
        let mut polygon = Vec::with_capacity(16);
        let mut scratch = Vec::with_capacity(16);
        for triangle in mesh.indices.as_chunks::<3>().0 {
            let vertices = triangle.map(|i| mesh.vertices[i as usize]);
            let [a, b, c] = vertices.map(|v| v.pos);
            let mut bounds = Rect::from_two_pos(a, b);
            bounds.extend_with(c);
            let bounds = bounds.intersect(visible);
            if !bounds.is_positive() || bounds.clamp(self.center).distance_sq(self.center) >= self.outer * self.outer {
                continue;
            }
            if [a, b, c].into_iter().all(|p| p.distance_sq(self.center) <= self.inset_sq) {
                append_polygon(&mut output, &vertices);
                continue;
            }
            if self.inner > 0.0 {
                clip_polygon(&vertices, &self.core, &mut polygon, &mut scratch);
                append_polygon(&mut output, &polygon);
            }
            for rim in self.rim.iter().filter(|rim| rim.bounds.intersects(bounds)) {
                clip_polygon(&vertices, &rim.points, &mut polygon, &mut scratch);
                let inner = (rim.points[0] - self.center).dot(rim.normal);
                let outer = (rim.points[1] - self.center).dot(rim.normal);
                for v in &mut polygon {
                    let alpha = ((outer - (v.pos - self.center).dot(rim.normal)) / (outer - inner)).clamp(0.0, 1.0);
                    v.color = v.color.gamma_multiply(alpha);
                }
                append_polygon(&mut output, &polygon);
            }
        }
        output
    }
}

fn clip_polygon(vertices: &[Vertex], exact: &[Pos2], out: &mut Vec<Vertex>, scratch: &mut Vec<Vertex>) {
    out.clear();
    out.extend_from_slice(vertices);
    for (&a, &b) in exact.iter().zip(exact.iter().cycle().skip(1)) {
        let Some(&last) = out.last() else { return };
        let mut prev = last;
        let edge = b - a;
        let side = |p: Pos2| edge.x * (p.y - a.y) - edge.y * (p.x - a.x);
        let mut before = side(prev.pos);
        scratch.clear();
        for &next in out.iter() {
            let after = side(next.pos);
            if (before < 0.0) != (after < 0.0) {
                scratch.push(interpolate(prev, next, before / (before - after)));
            }
            if after >= 0.0 { scratch.push(next); }
            (prev, before) = (next, after);
        }
        std::mem::swap(out, scratch);
    }
}

fn interpolate(a: Vertex, b: Vertex, t: f32) -> Vertex {
    let c = a.color.to_array();
    let d = b.color.to_array();
    let [r, g, blue, alpha] = std::array::from_fn(|i| egui::lerp(c[i] as f32..=d[i] as f32, t).round() as u8);
    Vertex { pos: a.pos.lerp(b.pos, t), uv: a.uv.lerp(b.uv, t), color: Color32::from_rgba_premultiplied(r, g, blue, alpha) }
}

fn append_polygon(out: &mut Mesh, polygon: &[Vertex]) {
    if polygon.len() < 3 { return; }
    let start = out.vertices.len() as u32;
    out.vertices.extend_from_slice(polygon);
    for i in 1..polygon.len() as u32 - 1 {
        out.add_triangle(start, start + i, start + i + 1);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use egui::pos2;

    fn triangle(points: [Pos2; 3]) -> Mesh {
        let mut mesh = Mesh::default();
        mesh.vertices.extend(points.map(|pos| Vertex {
            pos,
            uv: pos2(pos.x / 20.0 + 0.5, pos.y / 20.0 + 0.5),
            color: Color32::WHITE,
        }));
        mesh.add_triangle(0, 1, 2);
        mesh
    }

    #[test]
    fn culls_outside_triangles_and_preserves_the_opaque_core() {
        let c = Circle::new(Pos2::ZERO, 10.0, 1.0);
        let inside = triangle([pos2(-2.0, -1.0), pos2(1.0, -2.0), pos2(1.0, 2.0)]);
        let outside = triangle([pos2(15.0, 14.0), pos2(16.0, 14.0), pos2(15.0, 15.0)]);
        let kept = c.clip_mesh(inside, Rect::EVERYTHING);
        assert_eq!(kept.indices.len(), 3);
        assert!(kept.vertices.iter().all(|v| v.color == Color32::WHITE));
        assert!(c.clip_mesh(outside, Rect::EVERYTHING).indices.is_empty());
    }

    #[test]
    fn crossing_triangle_retains_affine_uv_and_gains_a_feather() {
        let c = Circle::new(Pos2::ZERO, 10.0, 1.0);
        let crossing = triangle([pos2(-1.0, -0.1), pos2(9.9, 0.1), pos2(11.0, 1.0)]);
        let mesh = c.clip_mesh(crossing, Rect::EVERYTHING);
        assert!(mesh.vertices.iter().any(|v| v.color.a() > 0 && v.color.a() < 255));
        for v in mesh.vertices {
            let wanted = pos2(v.pos.x / 20.0 + 0.5, v.pos.y / 20.0 + 0.5);
            assert!(v.uv.distance(wanted) < 0.00001);
            assert!(v.pos.distance(Pos2::ZERO) <= 10.5001);
        }
    }

    #[test]
    fn rounded_mask_keeps_straight_sides_and_excludes_the_rounded_corner() {
        let rect = Rect::from_min_max(pos2(-10.0, -6.0), pos2(10.0, 6.0));
        let mask = Circle::rounded(rect, 4.0, 1.0);
        let corner = triangle([pos2(10.0, 6.0), pos2(10.2, 6.0), pos2(10.0, 6.2)]);
        assert!(mask.clip_mesh(corner, Rect::EVERYTHING).indices.is_empty());
        let side = triangle([pos2(-1.0, 5.0), pos2(0.0, 6.0), pos2(2.0, 7.0)]);
        let clipped = mask.clip_mesh(side, Rect::EVERYTHING);
        assert!(clipped.vertices.iter().any(|v| v.color.a() > 0 && v.color.a() < 255));
        assert!(clipped.vertices.iter().all(|v| v.pos.y <= 6.5001));
    }

    #[test]
    fn rim_normals_point_outward_and_panels_share_their_edges() {
        let c = Circle::new(pos2(17.0, 22.0), 10.0, 0.5);
        for (i, edge) in c.rim.iter().enumerate() {
            let next = &c.rim[(i + 1) % c.rim.len()];
            assert_eq!(edge.points[3], next.points[0]);
            assert_eq!(edge.points[2], next.points[1]);
            let inner = (edge.points[0] - c.center).dot(edge.normal);
            let outer = (edge.points[1] - c.center).dot(edge.normal);
            assert!(inner > 0.0 && outer > inner);
        }
    }
}
