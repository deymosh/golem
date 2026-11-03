//! Pages that build their content with JavaScript, rendered in this
//! machine's browser through golem's proxy, against the real web. Ignored by
//! default (it needs a browser and the network): run with
//! `./scripts/cargo.sh test --test render_live -- --ignored --nocapture`.

use std::time::{Duration, Instant};

use golem_core::webtools;

/// The pages measured for the decision, with a phrase each one's rendered
/// content holds; static text alone holds none of them.
const PAGES: &[(&str, &str)] = &[
    ("https://crates.io/crates/serde", "serialization framework"),
    ("https://developer.apple.com/documentation/swift/array", "ordered, random-access collection"),
    ("https://bsky.app/profile/bsky.app", "followers"),
    ("https://open.spotify.com/artist/0OdUWJ0sBjDrqHygGUXeCF", "monthly listeners"),
];

#[test]
#[ignore]
fn javascript_pages_render_through_the_proxy() {
    let home = std::env::temp_dir().join("golem-render-live");
    let browser = golem::render::find().expect("a Chromium-family browser on this machine");
    println!("browser: {}", browser.display());
    let mut rendered = 0;
    for (url, phrase) in PAGES {
        let started = Instant::now();
        match golem::render::render(&home, url, Duration::from_secs(20)) {
            Ok(done) => {
                let page = webtools::page_from_html(&done.dom, webtools::PART_CHARS);
                let text = page.parts.join("\n\n");
                let found = text.to_lowercase().contains(&phrase.to_lowercase());
                println!(
                    "{url}: {} ms, {} chars, scripted {}, refused {}, phrase {found}",
                    started.elapsed().as_millis(),
                    text.chars().count(),
                    page.scripted,
                    done.refused
                );
                if found && !page.scripted {
                    rendered += 1;
                }
            }
            Err(message) => println!("{url}: {} ms, not rendered: {message}", started.elapsed().as_millis()),
        }
    }
    if let Ok(dump) = std::env::var("GOLEM_RENDER_DUMP") {
        if let Ok(done) = golem::render::render(&home, &dump, Duration::from_secs(20)) {
            let page = webtools::page_from_html(&done.dom, webtools::PART_CHARS);
            println!("--- {dump}\n{}", page.parts.join("\n\n").chars().take(3000).collect::<String>());
        }
    }
    let left: Vec<_> = std::fs::read_dir(golem::paths::browser_profiles(&home))
        .map(|dir| dir.flatten().map(|e| e.path()).collect())
        .unwrap_or_default();
    assert!(left.is_empty(), "profiles left behind: {left:?}");
    assert!(rendered >= 3, "{rendered} of {} pages rendered with their content", PAGES.len());
}

#[test]
#[ignore]
fn the_browser_never_reaches_this_machine() {
    // A listener on the loopback that must see no connection: the browser
    // is pointed straight at it, and the proxy is its only way out.
    let trap = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
    trap.set_nonblocking(true).expect("nonblocking");
    let port = trap.local_addr().expect("addr").port();
    let home = std::env::temp_dir().join("golem-render-live");
    let done = golem::render::render(&home, &format!("http://127.0.0.1:{port}/"), Duration::from_secs(20));
    println!("{done:?}");
    assert!(trap.accept().is_err(), "the browser reached the loopback");
    // The proxy's refusal leaves the browser no page; the error counts it.
    let said = done.expect_err("a refused page renders nothing");
    assert!(said.contains("refused"), "the proxy refused nothing: {said}");
}
