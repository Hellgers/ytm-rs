//! The four main pages share one vertical position. Settings is a sheet over that stack.

use std::rc::Rc;

use super::{LibTab, Page};

#[derive(Clone, Copy, Default)]
struct Slide {
    value: f32,
    from: f32,
    to: f32,
    elapsed: f32,
}

impl Slide {
    fn snap(&mut self, value: f32) {
        *self = Self { value, from: value, to: value, elapsed: 0.0 };
    }

    fn advance(&mut self, target: f32, dt: f32, secs: f32) {
        if self.to != target {
            self.from = self.value;
            self.to = target;
            self.elapsed = 0.0;
        }
        if self.value != target {
            self.elapsed += dt;
            let distance = (target - self.from).abs();
            // A short reversal has less ground to cover; a three-page jump still shows both stops.
            let t = (self.elapsed / (secs * distance.powf(0.65))).min(1.0);
            let eased = (t * t * (3.0 - 2.0 * t)).clamp(0.0, 1.0);
            self.value = if t >= 1.0 { target } else {
                (self.from + (target - self.from) * eased).clamp(target.min(self.from), target.max(self.from))
            };
        }
    }
}

pub(super) fn section(page: &Page) -> Option<usize> {
    match page {
        Page::Home => Some(0),
        Page::Explore => Some(1),
        Page::Library(_) => Some(2),
        Page::Jam => Some(3),
        _ => None,
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub(super) enum PlaylistMotionKind {
    Enter,
    Exit,
    Swap { direction: f32 },
}

#[derive(Clone)]
pub(super) struct PlaylistFrame {
    pub from: Page,
    pub to: Page,
    pub progress: f32,
    pub kind: PlaylistMotionKind,
}

#[derive(Clone)]
pub(super) struct Visit {
    pub page: Page,
    pub via_sidebar: bool,
    id: u64,
    cover: Option<Rc<CoverLink>>,
}

#[derive(Clone)]
struct CoverLink {
    source: Visit,
    origin: CoverOrigin,
}

impl Visit {
    pub fn initial() -> Self {
        Self { page: Page::Home, via_sidebar: false, id: 0, cover: None }
    }

    fn same(&self, other: &Self) -> bool {
        self.id == other.id && self.page == other.page
    }

    fn sidebar_playlist(&self) -> bool {
        self.via_sidebar && matches!(self.page, Page::Playlist(_))
    }

    pub fn forget_cover_from(&mut self, gone: &Page) {
        if let Some(cover) = &mut self.cover {
            if cover.source.page == *gone {
                self.cover = None;
            } else if cover.source.cover.is_some() {
                Rc::make_mut(cover).source.forget_cover_from(gone);
            }
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum Route {
    Plain,
    Sidebar,
    Back,
    Forward,
}

impl Route {
    fn sidebar_motion(self, from: &Visit, to: &Visit) -> bool {
        match self {
            Self::Plain => false,
            Self::Sidebar => true,
            Self::Back => from.via_sidebar,
            Self::Forward => to.via_sidebar,
        }
    }
}

struct PlaylistSlide {
    frame: PlaylistFrame,
    sides: [bool; 2],
    slide: Slide,
    target: f32,
}

impl PlaylistSlide {
    fn new(from: &Page, to: &Page, sides: [bool; 2], rank: impl Fn(&Page) -> Option<usize>) -> Option<Self> {
        if from == to { return None; }
        let kind = match sides {
            [false, true] => PlaylistMotionKind::Enter,
            [true, false] => PlaylistMotionKind::Exit,
            [true, true] => PlaylistMotionKind::Swap {
                direction: if rank(from).zip(rank(to)).is_some_and(|(a, b)| a > b) { -1.0 } else { 1.0 },
            },
            [false, false] => return None,
        };
        let frame = PlaylistFrame { from: from.clone(), to: to.clone(), progress: 0.0, kind };
        Some(Self { frame, sides, slide: Slide::default(), target: 1.0 })
    }
}

#[derive(Clone)]
pub(super) struct CoverOrigin {
    pub widget_id: egui::Id,
    pub rect: egui::Rect,
    pub radius: f32,
    pub thumbs: Vec<crate::model::Thumb>,
    pub zoom: f32,
}

#[derive(Clone)]
pub(super) struct CoverFrame {
    pub from: Page,
    pub to: Page,
    pub origin: CoverOrigin,
    pub progress: f32,
    pub route_id: u64,
    pub returning: bool,
    pub reset_header_scroll: bool,
}

struct CoverSlide {
    frame: CoverFrame,
    endpoints: [Visit; 2],
    slide: Slide,
    ready: bool,
}

impl CoverSlide {
    fn new(source: &Visit, collection: &Visit, origin: &CoverOrigin, returning: bool, fresh: bool) -> Self {
        let frame = CoverFrame {
            from: source.page.clone(),
            to: collection.page.clone(),
            origin: origin.clone(),
            progress: if returning { 1.0 } else { 0.0 },
            route_id: collection.id,
            returning,
            reset_header_scroll: fresh,
        };
        let mut slide = Slide::default();
        slide.snap(frame.progress);
        Self { frame, endpoints: [source.clone(), collection.clone()], slide, ready: returning }
    }

    fn target(&self) -> f32 {
        if self.frame.returning { 0.0 } else { 1.0 }
    }

    fn destination(&self) -> &Visit {
        &self.endpoints[self.target() as usize]
    }

    fn departure(&self) -> &Visit {
        &self.endpoints[1 - self.target() as usize]
    }
}

pub(super) struct Navigation {
    /// The page behind Settings, including detail pages reached outside the main stack.
    pub base: Page,
    library: LibTab,
    main: Slide,
    settings: Slide,
    last: Option<f64>,
    base_sidebar_playlist: bool,
    playlist: Option<PlaylistSlide>,
    pending_playlist: Option<PlaylistSlide>,
    cover: Option<CoverSlide>,
    pending_cover: Option<CoverSlide>,
    next_visit: u64,
    settings_destination: bool,
}

impl Default for Navigation {
    fn default() -> Self {
        Self { base: Page::Home, library: LibTab::Playlists, main: Slide::default(), settings: Slide::default(), last: None, base_sidebar_playlist: false, playlist: None, pending_playlist: None, cover: None, pending_cover: None, next_visit: 1, settings_destination: false }
    }
}

impl Navigation {
    pub fn new_visit(&mut self, from: &Visit, page: Page, intent: Route, cover: Option<CoverOrigin>, stacked: bool) -> Visit {
        let via_sidebar = intent == Route::Sidebar;
        if stacked && via_sidebar
            && let Some(link) = &from.cover
            && (link.source.page == page
                || matches!(link.source.page, Page::Library(_)) && page == Page::Library(LibTab::Playlists))
        {
            let mut source = link.source.clone();
            source.via_sidebar = true;
            return source;
        }
        let cover = cover.filter(|origin| {
            stacked && self.settings.value == 0.0
                && from.page != Page::Settings && !origin.thumbs.is_empty() && origin.rect.is_positive()
                && matches!(page, Page::Album(_) | Page::Playlist(_))
        }).map(|origin| Rc::new(CoverLink { source: from.clone(), origin }));
        let id = self.next_visit;
        self.next_visit += 1;
        Visit { page, via_sidebar, id, cover }
    }

    pub fn cancel_covers(&mut self) {
        self.cover = None;
        self.pending_cover = None;
    }

    pub fn route(&mut self, from: &Visit, to: &Visit, stacked: bool, intent: Route, rank: impl Fn(&Page) -> Option<usize>) {
        if !stacked || to.page == Page::Settings || from.page == Page::Settings || self.settings.value > 0.0 {
            self.cancel_covers();
        }
        if to.page == Page::Settings {
            self.settings_destination = false;
            self.playlist = None;
            self.pending_playlist = None;
            if from.page != Page::Settings {
                self.base = from.page.clone();
                self.base_sidebar_playlist = from.sidebar_playlist();
            }
            return;
        }
        if from.page == Page::Settings || self.settings.value > 0.0 {
            self.playlist = None;
            self.pending_playlist = None;
            self.settings_destination = true;
            self.base = to.page.clone();
            self.base_sidebar_playlist = to.sidebar_playlist();
            if let Some(index) = section(&to.page) { self.main.snap(index as f32); }
            return;
        }
        if from.page != to.page {
            self.settings_destination = false;
        }
        let new_sidebar = to.sidebar_playlist();
        self.base_sidebar_playlist = new_sidebar;
        if from.page == to.page {
            if let Some(pending) = &mut self.pending_playlist {
                pending.sides[1] = new_sidebar;
            } else if let Some(active) = &mut self.playlist {
                active.sides[active.target as usize] = new_sidebar;
            }
            return;
        }
        self.pending_cover = None;
        if stacked && self.route_cover(from, to, intent) {
            return;
        }

        // An unrelated third route starts from the endpoint of the cover already on screen.
        let origin = match &self.cover {
            Some(active) if active.ready => active.destination().clone(),
            Some(active) => {
                let source = active.departure().clone();
                self.cover = None;
                source
            }
            None => from.clone(),
        };
        let sidebar_exit = intent == Route::Sidebar && origin.cover.is_some() && !origin.sidebar_playlist();
        let make_playlist = |origin: &Page, old_sidebar: bool| {
            let sides = [old_sidebar || sidebar_exit, new_sidebar];
            let mut slide = PlaylistSlide::new(origin, &to.page, sides, &rank)?;
            if sidebar_exit {
                slide.frame.kind = PlaylistMotionKind::Exit;
                slide.sides[0] = old_sidebar;
            }
            Some(slide)
        };
        if !stacked || !(sidebar_exit || intent.sidebar_motion(&origin, to)) {
            self.playlist = None;
            self.pending_playlist = None;
            return;
        }
        if let Some(active) = &mut self.playlist {
            if self.cover.is_some() {
                // This playlist motion is queued behind the shared cover and has not started.
                self.playlist = make_playlist(&origin.page, origin.sidebar_playlist());
                self.pending_playlist = None;
                return;
            }
            for (i, endpoint) in [&active.frame.from, &active.frame.to].into_iter().enumerate() {
                if *endpoint == to.page && active.sides[i] == new_sidebar {
                    active.target = i as f32;
                    self.pending_playlist = None;
                    return;
                }
            }
            let i = active.target as usize;
            let endpoint = if i == 0 { &active.frame.from } else { &active.frame.to };
            self.pending_playlist = make_playlist(endpoint, active.sides[i]);
            if self.pending_playlist.is_none() { self.playlist = None; }
        } else {
            self.playlist = make_playlist(&origin.page, origin.sidebar_playlist());
        }
    }

    pub fn settings_destination_active(&self) -> bool {
        self.settings_destination
    }

    fn route_cover(&mut self, from: &Visit, to: &Visit, intent: Route) -> bool {
        let (source, collection, link, returning) = if let Some(link) = &from.cover
            && to.same(&link.source)
        {
            (to, from, link, true)
        } else if let Some(link) = &to.cover
            && from.same(&link.source)
        {
            (from, to, link, false)
        } else {
            return false;
        };

        if let Some(active) = &mut self.cover
            && active.frame.route_id == collection.id
        {
            active.frame.returning = returning;
            if returning {
                active.frame.reset_header_scroll = false;
            }
            self.playlist = None;
            self.pending_playlist = None;
            return true;
        }

        let next = CoverSlide::new(source, collection, &link.origin, returning, intent == Route::Plain);
        if let Some(active) = &self.cover {
            if !active.ready || !active.destination().same(from) { return false; }
            self.pending_cover = Some(next);
            self.playlist = None;
            self.pending_playlist = None;
            return true;
        } else if let Some(active) = self.pending_playlist.as_ref().or(self.playlist.as_ref()) {
            let endpoint = if active.target == 0.0 { &active.frame.from } else { &active.frame.to };
            if *endpoint != from.page { return false; }
            self.pending_cover = Some(next);
            return true;
        }
        self.cover = Some(next);
        true
    }

    pub fn cover_loading(&mut self, ready: bool, failed: bool) {
        if let Some(cover) = &mut self.cover {
            if failed && !ready && !cover.ready && !cover.frame.returning {
                self.cover = None;
            } else {
                cover.ready |= ready;
            }
        }
    }

    pub fn cover_lands_on(&self, page: &Page) -> bool {
        self.pending_cover.as_ref().or(self.cover.as_ref())
            .is_some_and(|cover| cover.destination().page == *page)
    }

    pub fn cover_destination(&self) -> Option<&Page> {
        self.cover.as_ref().map(|cover| &cover.frame.to)
    }

    pub fn cover_frame(&self) -> Option<CoverFrame> {
        self.cover.as_ref().map(|cover| {
            let mut frame = cover.frame.clone();
            frame.progress = cover.slide.value;
            frame
        })
    }

    pub fn sidebar_playlist_active(&self) -> bool {
        self.base_sidebar_playlist
    }

    pub fn playlist_frame(&self) -> Option<PlaylistFrame> {
        self.playlist.as_ref().map(|active| {
            let mut frame = active.frame.clone();
            frame.progress = active.slide.value;
            frame
        })
    }

    pub fn update(&mut self, page: &Page, now: f64) -> bool {
        let opening = *page == Page::Settings;
        let sheet = if opening { 1.0 } else { 0.0 };
        let index = section(page);
        if let Page::Library(tab) = page { self.library = *tab; }
        if !opening {
            if let Some(index) = index
                && (self.last.is_none() || section(&self.base).is_none() || self.base != *page && self.settings.value == 1.0)
            {
                self.main.snap(index as f32);
            }
            if self.base != *page { self.base = page.clone(); }
        }
        let Some(last) = self.last.replace(now) else {
            self.settings.snap(sheet);
            return false;
        };
        // Sampling twice at the same egui time leaves both views and the rail at the same position.
        let dt = (now - last).clamp(0.0, 1.0 / 24.0) as f32;
        if let Some(index) = section(&self.base) { self.main.advance(index as f32, dt, 0.48); }
        self.settings.advance(sheet, dt, 0.35);
        if let Some(cover) = &mut self.cover {
            let target = cover.target();
            if cover.ready { cover.slide.advance(target, dt, 0.38); }
            if cover.slide.value == target { self.cover = self.pending_cover.take(); }
        } else if let Some(active) = &mut self.playlist {
            active.slide.advance(active.target, dt, 0.38);
            if active.slide.value == active.target {
                self.playlist = self.pending_playlist.take();
            }
        }
        if self.cover.is_none() && self.playlist.is_none() {
            self.cover = self.pending_cover.take();
        }
        self.cover.as_ref().is_some_and(|cover| cover.ready) || self.playlist.is_some() || (section(&self.base).is_some() && self.main.value != self.main.to) || self.settings.value != sheet
    }

    pub fn position(&self) -> f32 { self.main.value }
    pub fn sheet(&self) -> f32 { self.settings.value }

    pub fn page_at(&self, index: usize) -> Page {
        match index {
            0 => Page::Home,
            1 => Page::Explore,
            2 => Page::Library(self.library),
            3 => Page::Jam,
            _ => unreachable!(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn visit(page: Page, via_sidebar: bool) -> Visit {
        Visit { page, via_sidebar, ..Visit::initial() }
    }

    #[test]
    fn playlist_routes_use_the_actual_origin_and_reverse_the_same_frame() {
        let mut nav = Navigation::default();
        let detail = visit(Page::Album("album".into()), false);
        let playlist = visit(Page::Playlist("p".into()), true);
        nav.update(&detail.page, 0.0);
        nav.route(&detail, &playlist, true, Route::Sidebar, |_| None);
        nav.update(&playlist.page, 0.03);
        let turn = nav.playlist_frame().unwrap();
        assert_eq!(turn.from, detail.page);
        assert_eq!(turn.to, playlist.page);
        assert_eq!(turn.kind, PlaylistMotionKind::Enter);
        nav.route(&playlist, &detail, true, Route::Sidebar, |_| None);
        nav.update(&detail.page, 0.03);
        assert_eq!(nav.playlist_frame().unwrap().progress, turn.progress);
        nav.update(&detail.page, 0.04);
        assert!(nav.playlist_frame().is_none_or(|frame| frame.progress < turn.progress));
    }

    #[test]
    fn playlist_swap_uses_current_ranks_and_finishes_before_a_third_target() {
        let mut nav = Navigation::default();
        let a = visit(Page::Playlist("a".into()), true);
        let b = visit(Page::Playlist("b".into()), true);
        let home = visit(Page::Home, true);
        nav.update(&a.page, 0.0);
        nav.route(&a, &b, true, Route::Sidebar, |page| if *page == a.page { Some(2) } else { Some(0) });
        nav.update(&b.page, 0.03);
        assert_eq!(nav.playlist_frame().unwrap().kind, PlaylistMotionKind::Swap { direction: -1.0 });
        nav.route(&b, &home, true, Route::Sidebar, |_| None);
        assert_eq!(nav.playlist_frame().unwrap().to, b.page);
        for i in 2..16 {
            nav.update(&home.page, f64::from(i) * 0.03);
        }
        let exit = nav.playlist_frame().unwrap();
        assert_eq!(exit.from, b.page);
        assert_eq!(exit.to, home.page);
        assert_eq!(exit.kind, PlaylistMotionKind::Exit);
    }

    #[test]
    fn settings_uses_its_playlist_backdrop_and_tile_navigation_remains_plain() {
        let mut nav = Navigation::default();
        let home = visit(Page::Home, false);
        let playlist = visit(Page::Playlist("a".into()), true);
        let settings = visit(Page::Settings, false);
        nav.update(&home.page, 0.0);
        nav.route(&home, &playlist, true, Route::Sidebar, |_| None);
        nav.update(&playlist.page, 0.03);
        assert!(nav.playlist_frame().is_some());
        nav.route(&playlist, &settings, true, Route::Plain, |_| None);
        nav.update(&settings.page, 0.04);
        let sheet = nav.sheet();
        assert!(sheet > 0.0);
        assert_eq!(nav.base, playlist.page);
        assert!(nav.playlist_frame().is_none());
        nav.route(&settings, &playlist, true, Route::Plain, |_| None);
        nav.update(&playlist.page, 0.05);
        assert!(nav.sheet() < sheet);
        let tile = visit(Page::Playlist("tile".into()), false);
        nav.route(&playlist, &tile, true, Route::Plain, |_| None);
        assert!(nav.playlist_frame().is_none());
    }

    #[test]
    fn exiting_settings_starts_from_the_actual_backdrop() {
        let mut nav = Navigation::default();
        let playlist = visit(Page::Playlist("a".into()), true);
        let settings = visit(Page::Settings, false);
        let explore = visit(Page::Explore, true);
        nav.update(&playlist.page, 0.0);
        nav.route(&playlist, &settings, true, Route::Plain, |_| None);
        for i in 1..14 {
            nav.update(&settings.page, f64::from(i) * 0.03);
        }
        assert_eq!(nav.sheet(), 1.0);
        assert!(nav.sidebar_playlist_active());
        nav.route(&settings, &explore, true, Route::Sidebar, |_| None);
        nav.update(&explore.page, 0.42);
        assert!(nav.playlist_frame().is_none());
        assert!(nav.settings_destination_active());
        assert_eq!(nav.base, explore.page);
        assert_eq!(nav.position(), 1.0);
        assert!(!nav.sidebar_playlist_active());
        assert!(nav.sheet() < 1.0);
    }

    #[test]
    fn selecting_the_current_tile_playlist_promotes_it_to_sidebar_without_a_slide() {
        let mut nav = Navigation::default();
        let tile = visit(Page::Playlist("a".into()), false);
        let sidebar = visit(tile.page.clone(), true);
        let other = visit(Page::Playlist("b".into()), true);
        nav.update(&tile.page, 0.0);

        nav.route(&tile, &sidebar, true, Route::Plain, |_| None);
        assert!(nav.sidebar_playlist_active());
        assert!(nav.playlist_frame().is_none());

        nav.route(&sidebar, &other, true, Route::Sidebar, |page| Some(usize::from(*page == other.page)));
        assert_eq!(nav.playlist_frame().unwrap().kind, PlaylistMotionKind::Swap { direction: 1.0 });
    }

    fn cover_origin() -> CoverOrigin {
        CoverOrigin {
            widget_id: egui::Id::unique("cover-card"),
            rect: egui::Rect::from_min_size(egui::pos2(20.0, 40.0), egui::vec2(80.0, 80.0)),
            radius: 8.0,
            thumbs: vec![crate::model::Thumb { url: String::new(), width: 80, height: 80 }],
            zoom: 1.09,
        }
    }

    #[test]
    fn shared_cover_waits_for_destination_then_cleans_up() {
        let mut nav = Navigation::default();
        let home = Visit::initial();
        nav.update(&home.page, 0.0);
        let origin = cover_origin();
        let album = nav.new_visit(&home, Page::Album("album".into()), Route::Plain, Some(origin.clone()), true);
        let to = album.page.clone();
        nav.route(&home, &album, true, Route::Plain, |_| None);
        assert!(!nav.update(&to, 0.03));
        let waiting = nav.cover_frame().unwrap();
        assert_eq!(waiting.from, Page::Home);
        assert_eq!(waiting.to, to);
        assert_eq!(waiting.origin.rect, origin.rect);
        assert_eq!(waiting.origin.zoom, origin.zoom);
        assert_eq!(waiting.progress, 0.0);

        nav.cover_loading(true, false);
        assert!(nav.update(&to, 0.06));
        assert!(nav.cover_frame().unwrap().progress > 0.0);
        for i in 3..18 { nav.update(&to, f64::from(i) * 0.03); }
        assert!(nav.cover_frame().is_none());

        nav.route(&home, &album, true, Route::Forward, |_| None);
        nav.cover_loading(false, true);
        assert!(nav.cover_frame().is_none());
    }

    fn forward_cover(nav: &mut Navigation, source: &Visit, page: Page) -> Visit {
        let visit = nav.new_visit(source, page, Route::Plain, Some(cover_origin()), true);
        nav.route(source, &visit, true, Route::Plain, |_| None);
        nav.cover_loading(true, false);
        visit
    }

    fn settle_cover(nav: &mut Navigation, page: &Page, start: f64) {
        for frame in 1..=18 { nav.update(page, start + f64::from(frame) * 0.03); }
        assert!(nav.cover_frame().is_none());
    }

    #[test]
    fn settled_cover_back_and_forward_keep_the_original_geometry() {
        let mut nav = Navigation::default();
        let home = Visit::initial();
        nav.update(&home.page, 0.0);
        let album = forward_cover(&mut nav, &home, Page::Album("a".into()));
        settle_cover(&mut nav, &album.page, 0.0);

        nav.route(&album, &home, true, Route::Back, |_| None);
        let frame = nav.cover_frame().unwrap();
        assert_eq!(frame.origin.rect, cover_origin().rect);
        assert_eq!(frame.origin.widget_id, cover_origin().widget_id);
        assert_eq!(frame.progress, 1.0);
        assert!(frame.returning);
        assert!(!frame.reset_header_scroll);
        nav.update(&home.page, 0.57);
        let progress = nav.cover_frame().unwrap().progress;
        assert!(progress < 1.0);

        nav.route(&home, &album, true, Route::Forward, |_| None);
        let frame = nav.cover_frame().unwrap();
        assert_eq!(frame.progress, progress);
        assert!(!frame.returning);
        assert!(!frame.reset_header_scroll);
        nav.update(&album.page, 0.60);
        assert!(nav.cover_frame().is_none_or(|frame| frame.progress > progress));
    }

    #[test]
    fn nested_history_starts_the_next_reverse_at_the_shared_endpoint() {
        let mut nav = Navigation::default();
        let home = Visit::initial();
        nav.update(&home.page, 0.0);
        let a = forward_cover(&mut nav, &home, Page::Album("a".into()));
        settle_cover(&mut nav, &a.page, 0.0);
        let b = forward_cover(&mut nav, &a, Page::Playlist("b".into()));
        settle_cover(&mut nav, &b.page, 0.54);

        nav.route(&b, &a, true, Route::Back, |_| None);
        nav.update(&a.page, 1.11);
        let before = nav.cover_frame().unwrap();
        nav.route(&a, &home, true, Route::Back, |_| None);
        assert_eq!(nav.cover_frame().unwrap().route_id, before.route_id);
        assert_eq!(nav.cover_frame().unwrap().progress, before.progress);
        for tick in 1..=15 {
            nav.update(&home.page, 1.11 + f64::from(tick) * 0.03);
            if let Some(frame) = nav.cover_frame()
                && frame.route_id != before.route_id
            {
                assert_eq!(frame.from, home.page);
                assert_eq!(frame.to, a.page);
                assert!(frame.returning);
                return;
            }
        }
        panic!("nested Back must replay the outer cover");
    }

    #[test]
    fn sidebar_selects_exact_cover_origin_or_runs_center_out() {
        let mut nav = Navigation::default();
        let home = Visit::initial();
        let library = nav.new_visit(&home, Page::Library(LibTab::Albums), Route::Sidebar, None, true);
        nav.update(&library.page, 0.0);
        let album = forward_cover(&mut nav, &library, Page::Album("a".into()));
        settle_cover(&mut nav, &album.page, 0.0);
        let via_rail = nav.new_visit(&album, Page::Library(LibTab::Playlists), Route::Sidebar, None, true);
        assert!(via_rail.same(&library));
        nav.route(&album, &via_rail, true, Route::Sidebar, |_| None);
        assert!(nav.cover_frame().unwrap().returning);
        settle_cover(&mut nav, &library.page, 0.54);

        let unrelated_visit = nav.new_visit(&album, library.page.clone(), Route::Plain, None, true);
        nav.route(&album, &unrelated_visit, true, Route::Back, |_| None);
        assert!(nav.cover_frame().is_none());
        let explore = nav.new_visit(&album, Page::Explore, Route::Sidebar, None, true);
        nav.route(&album, &explore, true, Route::Sidebar, |_| None);
        assert!(nav.cover_frame().is_none());
        assert_eq!(nav.playlist_frame().unwrap().kind, PlaylistMotionKind::Exit);
    }

    #[test]
    fn settings_clears_queued_and_active_covers_without_erasing_history() {
        let mut nav = Navigation::default();
        let home = Visit::initial();
        nav.update(&home.page, 0.0);
        let a = forward_cover(&mut nav, &home, Page::Album("a".into()));
        nav.update(&a.page, 0.03);
        let b = nav.new_visit(&a, Page::Album("b".into()), Route::Plain, Some(cover_origin()), true);
        nav.route(&a, &b, true, Route::Plain, |_| None);
        assert!(nav.pending_cover.is_some());

        let settings = nav.new_visit(&b, Page::Settings, Route::Sidebar, None, true);
        nav.route(&b, &settings, true, Route::Sidebar, |_| None);
        nav.update(&settings.page, 0.06);
        assert!(nav.cover_frame().is_none());
        assert!(nav.pending_cover.is_none());
        nav.route(&settings, &b, true, Route::Back, |_| None);
        nav.route(&b, &a, true, Route::Back, |_| None);
        assert!(nav.cover_frame().is_none());
        assert!(nav.settings_destination_active());
        for frame in 3..20 { nav.update(&a.page, f64::from(frame) * 0.03); }
        nav.route(&a, &home, true, Route::Back, |_| None);
        assert!(nav.cover_frame().unwrap().returning);
    }

    #[test]
    fn changing_destination_during_settings_close_cannot_start_another_transition() {
        let mut nav = Navigation::default();
        let home = visit(Page::Home, false);
        let settings = visit(Page::Settings, true);
        let playlist = visit(Page::Playlist("p".into()), true);
        let album = visit(Page::Album("a".into()), false);
        nav.update(&home.page, 0.0);
        nav.route(&home, &playlist, true, Route::Sidebar, |_| None);
        nav.update(&playlist.page, 0.03);
        nav.route(&playlist, &settings, true, Route::Sidebar, |_| None);
        for i in 2..6 { nav.update(&settings.page, f64::from(i) * 0.03); }
        assert!(nav.sheet() > 0.0 && nav.sheet() < 1.0);
        assert!(nav.playlist_frame().is_none());

        nav.route(&settings, &home, true, Route::Sidebar, |_| None);
        nav.update(&home.page, 0.16);
        assert!(nav.settings_destination_active());
        let album = nav.new_visit(&home, album.page, Route::Plain, Some(cover_origin()), true);
        assert!(album.cover.is_none());
        nav.route(&home, &album, true, Route::Plain, |_| None);
        assert!(nav.cover_frame().is_none());
        for i in 6..30 { nav.update(&album.page, f64::from(i) * 0.03); }
        assert_eq!(nav.sheet(), 0.0);
        assert_eq!(nav.base, album.page);
        assert!(nav.settings_destination_active());
        assert!(nav.playlist_frame().is_none());
    }

    #[test]
    fn a_jump_crosses_each_page_and_a_reversal_starts_where_it_is() {
        let mut nav = Navigation::default();
        nav.update(&Page::Home, 0.0);
        let mut stops = Vec::new();
        for frame in 1..=80 {
            nav.update(&Page::Jam, frame as f64 / 60.0);
            let i = nav.position().round() as usize;
            if stops.last() != Some(&i) { stops.push(i); }
        }
        assert_eq!(stops, [0, 1, 2, 3]);
        for frame in 81..=100 { nav.update(&Page::Home, frame as f64 / 60.0); }
        let turn = nav.position();
        nav.update(&Page::Jam, 100.0 / 60.0);
        assert_eq!(nav.position(), turn);
        nav.update(&Page::Jam, 101.0 / 60.0);
        assert!(nav.position() > turn);
    }

    #[test]
    fn a_detail_page_leaving_the_stack_does_not_keep_it_animating() {
        let mut nav = Navigation::default();
        nav.update(&Page::Home, 0.0);
        nav.update(&Page::Jam, 0.02);
        assert!(!nav.update(&Page::Artist("a".to_owned()), 0.04));
    }

    #[test]
    fn a_nearly_finished_slide_never_crosses_the_last_page() {
        let mut slide = Slide::default();
        let duration = 3.0_f32.powf(0.65);
        slide.advance(3.0, duration * 0.99990046, 1.0);
        assert!(slide.value.ceil() <= 3.0);
    }

    #[test]
    fn settings_keeps_its_backdrop_and_reveals_the_new_destination() {
        let mut nav = Navigation::default();
        let detail = Page::Album("a".to_owned());
        nav.update(&detail, 0.0);
        for frame in 1..=30 { nav.update(&Page::Settings, frame as f64 / 60.0); }
        assert_eq!(nav.base, detail);
        assert_eq!(nav.sheet(), 1.0);
        nav.update(&Page::Library(LibTab::Downloads), 0.51);
        assert_eq!(nav.position(), 2.0);
        assert_eq!(nav.page_at(2), Page::Library(LibTab::Downloads));
        let turn = nav.sheet();
        nav.update(&Page::Settings, 0.51);
        assert_eq!(nav.sheet(), turn);
        nav.update(&Page::Settings, 0.52);
        assert!(nav.sheet() > turn);
    }
}
