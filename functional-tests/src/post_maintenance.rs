//! Functional (browser-driven) test for the per-post admin maintenance actions:
//! "Recalculate perceptual hash", "Regenerate thumbnail", and "Convert to JPEG XL".
//!
//! Drives a real headless Firefox via geckodriver using the WebDriver protocol
//! (the same protocol Selenium uses), through the pure-Rust `fantoccini` client.
//!
//! Prerequisites (see README in this directory):
//! - geckodriver listening on 127.0.0.1:4444
//! - oxibooru stack (nginx + server + postgres) reachable at BASE_URL
//! - an administrator account and a regular account, both with the password
//!   used below, and post #1 must exist (an image post)

use fantoccini::{ClientBuilder, Locator};
use serde_json::json;
use std::time::Duration;

const BASE_URL: &str = "http://127.0.0.1:8088";
const ADMIN_USER: &str = "admin";
const ADMIN_PASS: &str = "AdminPass123";
const REGULAR_USER: &str = "regularjoe";
const REGULAR_PASS: &str = "RegularPass123";
const POST_ID: &str = "1";

async fn new_client() -> fantoccini::Client {
    let mut caps = serde_json::map::Map::new();
    caps.insert(
        "moz:firefoxOptions".to_string(),
        json!({"args": ["-headless"]}),
    );
    ClientBuilder::rustls()
        .expect("failed to set up rustls")
        .capabilities(caps)
        .connect("http://127.0.0.1:4444")
        .await
        .expect("failed to connect to geckodriver (is it running on :4444?)")
}

async fn login(c: &fantoccini::Client, user: &str, pass: &str) {
    c.goto(&format!("{BASE_URL}/login")).await.unwrap();

    let name_input = c
        .wait()
        .at_most(Duration::from_secs(10))
        .for_element(Locator::Css("#login form input[name='name']"))
        .await
        .unwrap();
    name_input.send_keys(user).await.unwrap();

    let pass_input = c
        .find(Locator::Css("#login form input[name='password']"))
        .await
        .unwrap();
    pass_input.send_keys(pass).await.unwrap();

    let submit = c
        .find(Locator::Css("#login form input[type='submit']"))
        .await
        .unwrap();
    submit.click().await.unwrap();

    // Wait for the login form to be replaced (redirect to home page on success).
    c.wait()
        .at_most(Duration::from_secs(10))
        .for_element(Locator::Css("body:not(:has(#login))"))
        .await
        .unwrap();

    // The "auth" cookie (used by `loginFromCookies()` to restore the session on a
    // full page load/reload) is set asynchronously by a follow-up token-creation
    // request, slightly after the login form disappears. Wait for it before
    // navigating away with `goto()` (a full reload), or the next page will see an
    // anonymous session and redirect away from privileged routes.
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    loop {
        if c.get_named_cookie("auth").await.is_ok() {
            break;
        }
        if tokio::time::Instant::now() >= deadline {
            panic!("timed out waiting for the \"auth\" cookie to be set after login");
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

async fn dump_debug(c: &fantoccini::Client, name: &str) {
    let url = c.current_url().await.map(|u| u.to_string()).unwrap_or_default();
    println!("[debug:{name}] current_url = {url}");
    if let Ok(src) = c.source().await {
        let path = format!("/tmp/{name}.html");
        std::fs::write(&path, &src).ok();
        println!("[debug:{name}] wrote source to {path} ({} bytes)", src.len());
    }
    if let Ok(png) = c.screenshot().await {
        let path = format!("/tmp/{name}.png");
        std::fs::write(&path, &png).ok();
        println!("[debug:{name}] wrote screenshot to {path}");
    }
}

async fn wait_for_message(c: &fantoccini::Client, expected_substring: &str) -> String {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(15);
    loop {
        let messages = c
            .find_all(Locator::Css(".edit-sidebar .messages .message"))
            .await
            .unwrap();
        for msg in messages {
            let text = msg.text().await.unwrap();
            if text.contains(expected_substring) {
                return text;
            }
        }
        if tokio::time::Instant::now() >= deadline {
            panic!("Timed out waiting for a message containing {expected_substring:?}");
        }
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
}

#[tokio::main]
async fn main() {
    let mut failures = Vec::new();

    if let Err(e) = test_admin_maintenance_actions().await {
        failures.push(format!("test_admin_maintenance_actions: {e}"));
    }
    if let Err(e) = test_regular_user_does_not_see_maintenance_section().await {
        failures.push(format!("test_regular_user_does_not_see_maintenance_section: {e}"));
    }

    if failures.is_empty() {
        println!("\nAll functional tests passed.");
    } else {
        eprintln!("\n{} functional test(s) FAILED:", failures.len());
        for f in &failures {
            eprintln!("  - {f}");
        }
        std::process::exit(1);
    }
}

async fn test_admin_maintenance_actions() -> Result<(), String> {
    println!("=== test_admin_maintenance_actions ===");
    let c = new_client().await;

    login(&c, ADMIN_USER, ADMIN_PASS).await;
    println!("Logged in as administrator.");

    c.goto(&format!("{BASE_URL}/post/{POST_ID}/edit")).await.unwrap();

    // Sanity check: maintenance section and all three links are present for admins.
    if let Err(e) = c
        .wait()
        .at_most(Duration::from_secs(10))
        .for_element(Locator::Css(".edit-sidebar .maintenance"))
        .await
    {
        dump_debug(&c, "admin_edit_page").await;
        return Err(format!("maintenance section not found: {e}"));
    }
    println!("Maintenance section is visible.");

    for selector in [
        ".maintenance .recompute-phash",
        ".maintenance .regenerate-thumbnail",
        ".maintenance .convert-to-jxl",
    ] {
        c.find(Locator::Css(selector))
            .await
            .map_err(|e| format!("link {selector} not found: {e}"))?;
    }
    println!("All three maintenance links are present.");

    // 1. Recalculate perceptual hash.
    let recompute_link = c
        .find(Locator::Css(".maintenance .recompute-phash"))
        .await
        .unwrap();
    recompute_link.click().await.unwrap();
    let msg = wait_for_message(&c, "Perceptual hash recalculated").await;
    println!("recompute-phash -> {msg:?}");

    // 2. Regenerate thumbnail.
    let regen_link = c
        .find(Locator::Css(".maintenance .regenerate-thumbnail"))
        .await
        .unwrap();
    regen_link.click().await.unwrap();
    let msg = wait_for_message(&c, "Thumbnail regenerated").await;
    println!("regenerate-thumbnail -> {msg:?}");

    // 3. Convert to JPEG XL (irreversible, guarded by a confirm() dialog).
    let convert_link = c
        .find(Locator::Css(".maintenance .convert-to-jxl"))
        .await
        .unwrap();
    convert_link.click().await.unwrap();

    // Accept the native confirm() dialog.
    c.accept_alert()
        .await
        .map_err(|e| format!("expected a confirm() dialog for convert-to-jxl: {e}"))?;

    let msg = wait_for_message(&c, "Post converted to JPEG XL").await;
    println!("convert-to-jxl -> {msg:?}");

    // Verify the content URL now points to a .jxl file.
    let content_img = c
        .wait()
        .at_most(Duration::from_secs(10))
        .for_element(Locator::Css(".post-content img, .post-content video, .post-content source"))
        .await
        .map_err(|e| format!("post content element not found: {e}"))?;
    let src = content_img
        .attr("src")
        .await
        .unwrap()
        .unwrap_or_default();
    if !src.contains(".jxl") {
        return Err(format!("expected post content URL to end in .jxl, got {src:?}"));
    }
    println!("Post content URL updated to JXL: {src}");

    c.close().await.unwrap();
    println!("=== test_admin_maintenance_actions: PASS ===\n");
    Ok(())
}

async fn test_regular_user_does_not_see_maintenance_section() -> Result<(), String> {
    println!("=== test_regular_user_does_not_see_maintenance_section ===");
    let c = new_client().await;

    login(&c, REGULAR_USER, REGULAR_PASS).await;
    println!("Logged in as regular user.");

    c.goto(&format!("{BASE_URL}/post/{POST_ID}/edit")).await.unwrap();

    // Wait for the sidebar to render before asserting absence.
    if let Err(e) = c
        .wait()
        .at_most(Duration::from_secs(10))
        .for_element(Locator::Css(".edit-sidebar"))
        .await
    {
        dump_debug(&c, "regular_edit_page").await;
        return Err(format!("edit sidebar not found: {e}"));
    }

    let maintenance = c.find_all(Locator::Css(".edit-sidebar .maintenance")).await.unwrap();
    if !maintenance.is_empty() {
        return Err("maintenance section should not be visible to a regular user".to_string());
    }
    println!("Maintenance section correctly hidden from regular user.");

    c.close().await.unwrap();
    println!("=== test_regular_user_does_not_see_maintenance_section: PASS ===\n");
    Ok(())
}
