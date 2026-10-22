//! Pick the capture card. `auto` prefers a ReSpeaker-class USB array.

#[must_use]
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
pub fn select_capture_device(requested: &str, cards: &str) -> String {
    let requested = requested.trim();
    if !requested.is_empty() && !requested.eq_ignore_ascii_case("auto") {
        return requested.to_string();
    }
    for (index, id, kind) in parse_cards(cards) {
        let blob = format!("{id} {kind}").to_ascii_lowercase();
        if blob.contains("respeaker")
            || blob.contains("seeed")
            || blob.contains("xvf3800")
            || blob.contains("xvf-3800")
            || blob.contains("xmos")
        {
            return format!("hw:{index},0");
        }
    }
    "default".to_string()
}

#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
fn parse_cards(text: &str) -> Vec<(u32, String, String)> {
    let mut cards = Vec::new();
    for line in text.lines() {
        let Some(rest) = line.trim().strip_prefix(|ch: char| ch.is_ascii_digit()) else {
            continue;
        };
        let Some(index_token) = line.split_whitespace().next() else {
            continue;
        };
        let Ok(index) = index_token.parse::<u32>() else {
            continue;
        };
        let Some(id_start) = rest.find('[') else {
            continue;
        };
        let Some(id_end) = rest.find(']') else {
            continue;
        };
        let id = rest[id_start + 1..id_end].trim().to_string();
        let kind = rest[id_end + 1..]
            .trim()
            .trim_start_matches(':')
            .trim()
            .to_string();
        cards.push((index, id, kind));
    }
    cards
}

#[cfg(test)]
mod tests {
    use super::select_capture_device;

    const CARDS: &str = "\
 0 [vc4hdmi0       ]: vc4-hdmi - vc4-hdmi-0
 1 [ReSpeaker      ]: USB-Audio - ReSpeaker XVF3800
 2 [UAC2           ]: USB-Audio - USB Audio Device
";

    #[test]
    fn auto_picks_the_array_and_an_explicit_name_wins() {
        assert_eq!(select_capture_device("auto", CARDS), "hw:1,0");
        assert_eq!(select_capture_device("hw:2,0", CARDS), "hw:2,0");
        assert_eq!(
            select_capture_device("auto", " 0 [vc4hdmi0 ]: vc4-hdmi\n"),
            "default"
        );
    }
}
