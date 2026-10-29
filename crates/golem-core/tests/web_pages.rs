//! The web tools' reading of saved pages: a documentation page, a news
//! article, a forum thread, a page that only renders with JavaScript, and
//! DuckDuckGo's no-JavaScript results in both of the forms it answers with.
//! The pages are in `tests/fixtures/web/` (their sources are in its README).

use golem_core::html::{self, Document};
use golem_core::webtools;

fn fixture(name: &str) -> String {
    let path = format!("{}/tests/fixtures/web/{name}", env!("CARGO_MANIFEST_DIR"));
    std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("{path}: {e}"))
}

/// The page as the model reads its first part.
fn page(name: &str) -> (webtools::Page, String) {
    let page = webtools::page_from_html(&fixture(name));
    let text = webtools::fetch_text("https://example.test/", &page, 1);
    (page, text)
}

#[test]
fn a_docs_page_keeps_its_headings_code_tables_and_signatures() {
    let (page, text) = page("docs-python-json.html");
    assert_eq!(page.title, "json — JSON encoder and decoder — Python 3.15.2 documentation");
    assert!(text.contains("# `json` — JSON encoder and decoder\n"), "the header link's ¶ is gone");
    assert!(text.contains("```python3\n>>> import json\n>>> json.dumps(['foo', {'bar': ('baz', None, 1.0, 2)}])"));
    assert!(text.contains("| JSON | Python |\n| --- | --- |\n| object | dict |"));
    assert!(text.contains("json.dump(obj, fp, *, skipkeys=False"));
    assert!(text.contains("- obj (object) – The Python object to be serialized."));
    for noise in ["<", "Theme", "Navigation", "Table of Contents", "Report a bug"] {
        assert!(!text.contains(noise), "{noise:?} leaked into the page");
    }
    assert_eq!(page.parts.len(), 2, "about 26K characters is two parts");
    assert!(text.ends_with("--- end (part 1 of 2, call again with page=2) ---"));
}

#[test]
fn a_news_article_is_its_story_and_sources_without_the_wiki_around_it() {
    let (page, text) = page("news-wikinews.html");
    assert_eq!(page.parts.len(), 1);
    assert!(text.contains("# Boats carrying Kyoto students capsize off Okinawa, fatalities reported"));
    assert!(text.contains("On Monday, two boats carrying 21 people total capsized at Henoko Bay"));
    assert!(text.contains("## Sources\n\n- 2 dead, including teen girl"));
    for noise in ["Jump to", "Main menu", "Personal tools", "[edit]", "Categories", "Privacy policy"] {
        assert!(!text.contains(noise), "{noise:?} leaked into the article");
    }
}

#[test]
fn a_forum_thread_keeps_every_post_and_its_code() {
    let (_, text) = page("forum-rust-users.html");
    assert!(text.contains("# Elegantly/conveniantly filling arrays"));
    assert!(text.contains("```rust\n#[derive(Clone, Copy)]\nenum Field {"));
    assert!(text.contains("> error: the trait `core::marker::Copy` is not implemented"));
    let posts = text.matches(", 2015, ").count();
    assert_eq!(posts, 8, "eight posts, each with its byline");
}

#[test]
fn a_javascript_only_page_says_so_with_its_description() {
    let (page, text) = page("js-only-excalidraw.html");
    assert_eq!(page.title, "Excalidraw Whiteboard");
    assert!(text.contains("Description: Excalidraw is a virtual collaborative whiteboard tool"));
    assert!(text.contains("builds its content with JavaScript, which Golem's fetch does not run"));
}

#[test]
fn duckduckgo_results_come_back_with_their_real_addresses() {
    // The POST form links straight to the page.
    let posted = html::duckduckgo_results(&fixture("duckduckgo-post.html"), webtools::SEARCH_RESULTS);
    assert_eq!(posted.len(), 8);
    assert_eq!(posted[0].title, "read_to_string in std::fs - Rust");
    assert_eq!(posted[0].url, "https://doc.rust-lang.org/std/fs/fn.read_to_string.html");
    assert!(posted[0].snippet.starts_with("Reads the entire contents of a file into a string."));

    // The GET form wraps every link in DuckDuckGo's redirect.
    let got = html::duckduckgo_results(&fixture("duckduckgo-get.html"), webtools::SEARCH_RESULTS);
    assert_eq!(got.len(), 8);
    assert_eq!(got[0].url, "https://www.runnersworld.com/gear/a19663621/best-running-shoes/");
    assert_eq!(got[0].title, "The 15 Best Running Shoes of 2026, Tested by Runner's World");
    assert!(got.iter().all(|r| !r.url.contains("duckduckgo.com")), "{got:?}");
    assert!(got.iter().all(|r| r.snippet.chars().count() <= 301));
    assert!(!html::duckduckgo_blocked(&fixture("duckduckgo-get.html")));

    let text = webtools::search_text("best running shoes 2026", Some("2026-10-28"), &got);
    assert!(text.starts_with("Golem web search: \"best running shoes 2026\" (DuckDuckGo, 2026-10-28), 8 results\n\n1. "));
    assert!(text.len() < 4000, "about 1K tokens, not {} bytes", text.len());
}

#[test]
fn no_fixture_parses_to_markup() {
    for name in ["docs-python-json.html", "news-wikinews.html", "forum-rust-users.html", "js-only-excalidraw.html"] {
        let markdown = Document::parse(&fixture(name)).markdown();
        assert!(!markdown.contains("</") && !markdown.contains("<div"), "{name} left markup in");
    }
}
