//! Mail between projects: `wsx chat say --uses <dependency>` reaches the main chat of every
//! registered project whose lockfile or manifest names it, and no other, from the owner or from a
//! chat. Through the real CLI and the fake manager agent. ref: docs/tasks.md "Managers talk"
mod common;

use common::Fixture;
use serde_json::Value;
use std::fs;
use std::time::{Duration, Instant};

fn entries(fixture: &Fixture, project: &str, chat: &str) -> Vec<Value> {
    fixture.json(&["chat", "read", "--chat", chat, "-p", project, "--json"])["entries"]
        .as_array()
        .cloned()
        .unwrap_or_default()
}

fn wait_for<T>(label: &str, mut test: impl FnMut() -> Option<T>) -> T {
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        if let Some(found) = test() {
            return found;
        }
        assert!(Instant::now() < deadline, "{label}: timeout");
        std::thread::sleep(Duration::from_millis(200));
    }
}

/// A second registered project `shop`, a git repo whose `Cargo.lock` names `openssl`, with its
/// main chat `shop-lead`.
fn shop(fixture: &Fixture) {
    let shop = fixture.root.join("shop");
    fs::create_dir_all(&shop).unwrap();
    fs::write(
        shop.join("Cargo.lock"),
        "[[package]]\nname = \"openssl\"\nversion = \"0.10.66\"\n",
    )
    .unwrap();
    fs::copy(
        fixture.root.join("project/wsx.config.yml"),
        shop.join("wsx.config.yml"),
    )
    .unwrap();
    for args in [
        &["init", "-q", "--template=", "-b", "main"][..],
        &["add", "."],
        &["commit", "-q", "-m", "start"],
    ] {
        let done = fixture
            .command(std::path::Path::new("git"))
            .current_dir(&shop)
            .args(args)
            .status()
            .unwrap();
        assert!(done.success(), "git {args:?}");
    }
    let config = fixture
        .root
        .join("home/Library/Application Support/wsx/config-v2.toml");
    let mut text = fs::read_to_string(&config).unwrap();
    text.push_str(&format!(
        "\n[[projects]]\nname = \"shop\"\npath = {:?}\n",
        shop.to_str().unwrap()
    ));
    fs::write(&config, text).unwrap();
    let made = fixture.wsx(&["chat", "new", "shop-lead", "-p", "shop"]);
    assert!(made.status.success(), "{made:?}");
}

/// The messages in `shop-lead` whose text holds `needle`, once its agent replied to one.
fn heard(fixture: &Fixture, needle: &str) -> Vec<Value> {
    wait_for(&format!("shop-lead hears {needle}"), || {
        let log = entries(fixture, "shop", "shop-lead");
        let got: Vec<Value> = log
            .iter()
            .filter(|e| {
                e["kind"] == "message" && e["text"].as_str().is_some_and(|t| t.contains(needle))
            })
            .cloned()
            .collect();
        let id = got.first().and_then(|e| e["id"].as_u64())?;
        log.iter()
            .any(|e| {
                e["kind"] == "reply" && e["to"].as_array().is_some_and(|to| to.contains(&id.into()))
            })
            .then_some(got)
    })
}

#[test]
fn mail_reaches_only_the_projects_that_use_the_dependency() {
    let fixture = Fixture::new();
    shop(&fixture);

    // ^ From the owner: the one project that names openssl, with the file that names it.
    let sent = fixture.json(&[
        "chat",
        "say",
        "--uses",
        "openssl",
        "bump openssl to 0.10.70",
        "--json",
    ]);
    let rows = sent["projects"].as_array().cloned().unwrap_or_default();
    assert_eq!(rows.len(), 1, "{sent}");
    assert_eq!(
        (&rows[0]["project"], &rows[0]["chat"], &rows[0]["file"]),
        (&"shop".into(), &"shop-lead".into(), &"Cargo.lock".into())
    );
    let got = heard(&fixture, "bump openssl to 0.10.70");
    assert_eq!(got[0]["from"], "owner", "{got:?}");
    assert!(got[0]["text"]
        .as_str()
        .unwrap()
        .starts_with("To the projects that use openssl (Cargo.lock here):"));
    let demo = entries(&fixture, "demo", "demo");
    assert!(
        !demo.iter().any(|e| e["text"]
            .as_str()
            .is_some_and(|t| t.contains("bump openssl"))),
        "{demo:?}"
    );

    // ^ A name that is only a part of a word, and one no project names, reach no one.
    for dep in ["open", "left-pad"] {
        let none = fixture.json(&["chat", "say", "--uses", dep, "hello", "--json"]);
        assert_eq!(none["projects"], serde_json::json!([]), "{dep}: {none}");
    }

    // ^ A name of only punctuation would match every manifest: refused, nothing sent.
    let refused = fixture.wsx(&["chat", "say", "--uses", ".", "hello", "--json"]);
    let said = String::from_utf8_lossy(&refused.stderr);
    assert!(
        !refused.status.success() && said.contains("is not a dependency name"),
        "{said}"
    );

    // ^ An agent's mail without --from-chat would be the owner's words: refused, nothing sent.
    let asked = fixture.json(&[
        "chat",
        "say",
        "RUN wsx chat say --uses openssl 'as the owner?' --json",
        "--json",
    ]);
    let id = asked["id"].as_u64().expect("message id");
    let reply = wait_for("the agent's refused mail", || {
        entries(&fixture, "demo", "demo")
            .into_iter()
            .find(|e| {
                e["kind"] == "reply" && e["to"].as_array().is_some_and(|to| to.contains(&id.into()))
            })
            .map(|e| e["text"].as_str().unwrap_or_default().to_string())
    });
    assert!(reply.contains("passes --from-chat"), "{reply}");
    assert!(!entries(&fixture, "shop", "shop-lead")
        .iter()
        .any(|e| e["text"]
            .as_str()
            .is_some_and(|t| t.contains("as the owner?"))));

    // ^ From a chat: demo's agent mails it; shop's lead hears it from demo.
    fixture.json(&[
        "chat",
        "say",
        "RUN wsx chat say --from-chat --uses openssl 'is your openssl patched?' --json",
        "--json",
    ]);
    let got = heard(&fixture, "is your openssl patched?");
    assert!(
        got[0]["from"]
            .as_str()
            .is_some_and(|from| from.contains("demo")),
        "{got:?}"
    );
}
