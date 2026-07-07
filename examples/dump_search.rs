//! Parser-rot forensics: dump the raw JSON of an authenticated songs search so the
//! vendored/patched ytmapi-rs parse paths can be checked against what YouTube Music
//! actually returns today. Usage:
//!   cargo run --example dump_search -- "IU Celebrity" > /tmp/search.json

use ytmapi_rs::YtMusic;
use ytmapi_rs::query::SearchQuery;
use ytmapi_rs::query::search::{FilteredSearch, SongsFilter};

fn netscape_to_header(content: &str) -> String {
    let mut pairs = Vec::new();
    for raw in content.lines() {
        let line = raw.strip_prefix("#HttpOnly_").unwrap_or(raw);
        if line.starts_with('#') || line.trim().is_empty() {
            continue;
        }
        let fields: Vec<&str> = line.split('\t').collect();
        if fields.len() >= 7 && fields[0].contains("youtube.com") {
            pairs.push(format!("{}={}", fields[5].trim(), fields[6].trim()));
        }
    }
    format!("{};", pairs.join("; "))
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let query = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "IU Celebrity".to_owned());
    let home = std::env::var("HOME")?;
    let cookies = std::fs::read_to_string(format!("{home}/Music/yututui/cookies.txt"))?;
    let yt = YtMusic::from_cookie(netscape_to_header(&cookies)).await?;
    let q: SearchQuery<FilteredSearch<SongsFilter>> = query.as_str().into();
    let raw = yt
        .raw_json_query::<SearchQuery<FilteredSearch<SongsFilter>>>(&q)
        .await?;
    println!("{raw}");
    Ok(())
}
