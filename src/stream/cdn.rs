use serde_json::Value;

pub(crate) struct CdnFallback {
    pub(super) url: String,
    pub(super) after: u32,
}

impl CdnFallback {
    pub(crate) fn from_player(url: &str, response: &Value) -> Option<Self> {
        let config = &response["playerConfig"]["hamplayerConfig"]["chunkLoaderConfig"];
        if config["enableFallbackHost"] != true {
            return None;
        }
        let after = config["maxFailureAttemptsBeforeFallback"].as_u64()?;
        if !(1..=6).contains(&after) {
            return None;
        }

        if url.bytes().any(|b| b.is_ascii_control() || b.is_ascii_whitespace() || !b.is_ascii() || b == b'#') {
            return None;
        }
        let (authority, resource) = url.strip_prefix("https://")?.split_once('/')?;
        let query = resource.strip_prefix("videoplayback?")?;
        let (host, port) = authority.strip_suffix(":443").map_or((authority, ""), |host| (host, ":443"));
        let (server, suffix) = host.split_once('.')?;
        if !matches!(suffix, "googlevideo.com" | "a1.googlevideo.com") || server.len() > 63 {
            return None;
        }
        let (shard, current) = server.split_once("---")?;
        let (prefix, shard) = if let Some(shard) = shard.strip_prefix("rr") {
            ("rr", shard)
        } else {
            ("r", shard.strip_prefix('r')?)
        };
        if shard.is_empty() || !shard.bytes().all(|b| b.is_ascii_digit()) {
            return None;
        }

        let (mut mn, mut fvip, mut lsparams, mut lsig, mut sparams) = (None, None, None, None, None);
        for (name, value) in url::form_urlencoded::parse(query.as_bytes()) {
            let parameter = match name.as_ref() {
                "mn" => &mut mn,
                "fvip" => &mut fvip,
                "lsparams" => &mut lsparams,
                "lsig" => &mut lsig,
                "sparams" => &mut sparams,
                "fallback_count" => return None,
                _ => continue,
            };
            if parameter.replace(value).is_some() {
                return None;
            }
        }
        let lsparams = lsparams?;
        if lsparams.split(',').filter(|name| *name == "mn").count() != 1
            || lsparams.split(',').any(|name| name == "fallback_count")
            || sparams.is_some_and(|names| names.split(',').any(|name| name == "fallback_count"))
            || lsig?.is_empty()
        {
            return None;
        }

        // The web player takes the second mn node and uses fvip as its shard.
        let mn = mn?;
        let (first, second) = mn.split_once(',')?;
        if first != current || first == second || !is_node(first) || !is_node(second) {
            return None;
        }
        let fvip = fvip?;
        if fvip.len() > 2 || !fvip.bytes().all(|b| b.is_ascii_digit()) || !(1..=99).contains(&fvip.parse::<u8>().ok()?) {
            return None;
        }
        if prefix.len() + fvip.len() + 3 + second.len() > 63 {
            return None;
        }
        Some(Self {
            url: format!("https://{prefix}{fvip}---{second}.{suffix}{port}/videoplayback?{query}&fallback_count=1"),
            after: after as u32,
        })
    }
}

fn is_node(label: &str) -> bool {
    label.len() <= 63 && label.strip_prefix("sn-").is_some_and(|name| {
        name.split('-').all(|part| !part.is_empty() && part.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit()))
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    const QUERY: &str = "n=%2b%2F+raw&mn=sn-alpha%2csn-beta&fvip=7&lsparams=mn%2Cspc&lsig=fake%3D&sparams=itag%2Cid";
    const HOST: &str = "https://rr11---sn-alpha.googlevideo.com";

    fn config(enabled: bool, after: u32) -> Value {
        json!({"playerConfig": {"hamplayerConfig": {"chunkLoaderConfig": {
            "enableFallbackHost": enabled, "maxFailureAttemptsBeforeFallback": after,
        }}}})
    }

    #[test]
    fn a_player_fallback_preserves_the_resource_and_raw_query() {
        let response = config(true, 2);
        for (original, alternate) in [
            (HOST, "https://rr7---sn-beta.googlevideo.com"),
            ("https://r3---sn-alpha.a1.googlevideo.com:443", "https://r7---sn-beta.a1.googlevideo.com:443"),
        ] {
            let fallback = CdnFallback::from_player(&format!("{original}/videoplayback?{QUERY}"), &response).unwrap();
            assert_eq!(fallback.after, 2);
            assert_eq!(fallback.url, format!("{alternate}/videoplayback?{QUERY}&fallback_count=1"));
            assert!(CdnFallback::from_player(&fallback.url, &response).is_none());
        }
    }

    #[test]
    fn a_fallback_needs_explicit_player_config_and_its_signed_node_list() {
        let url = format!("{HOST}/videoplayback?{QUERY}");
        for response in [json!({}), config(false, 2), config(true, 0), config(true, 7)] {
            assert!(CdnFallback::from_player(&url, &response).is_none());
        }
        let mut response = config(true, 2);
        response["playerConfig"]["hamplayerConfig"]["chunkLoaderConfig"]["maxFailureAttemptsBeforeFallback"] = Value::Null;
        assert!(CdnFallback::from_player(&url, &response).is_none());

        let response = config(true, 2);
        for query in [
            QUERY.replace("lsparams=mn%2Cspc", "lsparams=spc"),
            QUERY.replace("lsig=fake%3D", "lsig="),
            QUERY.replace("&fvip=7", ""),
            QUERY.replace("fvip=7", "fvip=100"),
            QUERY.replace("sn-alpha%2csn-beta", "sn-other%2csn-beta"),
            QUERY.replace("sn-alpha%2csn-beta", "sn-alpha%2csn-alpha"),
            QUERY.replace("sn-alpha%2csn-beta", "sn-alpha%2csn-beta%2cextra"),
            QUERY.replace("sn-beta", "sn-beta%2Eevil"),
            QUERY.replace("sn-beta", "sn-beta%2Fpath"),
            QUERY.replace("sn-beta", "sn-%C3%A9vil"),
            QUERY.replace("spc", "fallback_count"),
            QUERY.replace("itag%2Cid", "itag%2Cfallback_count"),
            format!("{QUERY}&m%6e=sn-alpha%2Csn-other"),
            format!("{QUERY}&fvip=8"),
            format!("{QUERY}&fall%62ack_count=0"),
        ] {
            assert!(CdnFallback::from_player(&format!("{HOST}/videoplayback?{query}"), &response).is_none());
        }
    }

    #[test]
    fn a_fallback_rejects_other_origins_and_resources() {
        let response = config(true, 2);
        for url in [
            format!("http://rr11---sn-alpha.googlevideo.com/videoplayback?{QUERY}"),
            format!("https://rr11---sn-alpha.googlevideo.com:444/videoplayback?{QUERY}"),
            format!("https://user@rr11---sn-alpha.googlevideo.com/videoplayback?{QUERY}"),
            format!("https://rr11---sn-alpha.googlevideo.com.evil.test/videoplayback?{QUERY}"),
            format!("https://a1.googlevideo.com/videoplayback?{QUERY}"),
            format!("https://rr11---sn-alpha.b1.googlevideo.com/videoplayback?{QUERY}"),
            format!("{HOST}/elsewhere/videoplayback?{QUERY}"),
            format!("{HOST}/videoplayback/?{QUERY}"),
            format!("{HOST}/videoplayback?{QUERY}#fragment"),
        ] {
            assert!(CdnFallback::from_player(&url, &response).is_none());
        }
    }
}
