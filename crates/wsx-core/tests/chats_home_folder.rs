//! A chat's Home folder (`chats_home::set_folder`, `folder`, `sections`): set, read in the chat's
//! row, moved, cleared; the sections go by the first chat of each folder and the chats of no
//! folder come last; a folder with no chat is no section. Real files in a temp state folder.
//! ref: docs/web-client-tasks.md "Home"

use std::fs;
use wsx_core::task::chats;
use wsx_core::task::chats_home;

const PROJECT: &str = "demo-0a1b2c3d";

fn row_folders() -> Vec<(String, Option<String>)> {
    let mut rows: Vec<_> = chats::open_chats(PROJECT)
        .iter()
        .map(|dir| (dir.name().to_string(), chats_home::folder(dir)))
        .collect();
    rows.sort();
    rows
}

#[test]
fn a_folder_is_one_file_of_its_chat_and_goes_with_its_last_chat() {
    let state = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../.work")
        .join(format!("folder-{}", std::process::id()));
    let _ = fs::remove_dir_all(&state);
    fs::create_dir_all(&state).unwrap();
    let state = state.canonicalize().unwrap();
    // SAFETY: this file has one test; nothing else reads the environment meanwhile.
    unsafe { std::env::set_var("XDG_STATE_HOME", &state) };

    for name in ["wsx", "wsx-ui", "projk", "research"] {
        chats::create(PROJECT, name, None, None).unwrap();
    }
    assert!(row_folders().iter().all(|(_, folder)| folder.is_none()));

    // ^ Round trip: set, read from the chat's directory, moved, and "No folder" removes the file.
    chats_home::set_folder("wsx", Some("  Work ")).unwrap();
    chats_home::set_folder("wsx-ui", Some("Work")).unwrap();
    chats_home::set_folder("projk", Some("Personal")).unwrap();
    let dir = chats::open_chat("wsx").unwrap();
    assert_eq!(
        fs::read_to_string(dir.path.join(chats_home::FOLDER_FILE)).unwrap(),
        "Work"
    );
    assert_eq!(chats_home::folder(&dir).as_deref(), Some("Work"));
    chats_home::set_folder("wsx", Some("Clients")).unwrap();
    assert_eq!(chats_home::folder(&dir).as_deref(), Some("Clients"));

    // ^ The owner's order of the chats (here: projk, wsx, research, wsx-ui): a folder stands where
    // its first chat is, its chats keep their order, the chats of no folder come last.
    let ordered: Vec<_> = ["projk", "wsx", "research", "wsx-ui"]
        .iter()
        .map(|name| chats::open_chat(name).unwrap())
        .collect();
    let sections = chats_home::sections(ordered, chats_home::folder);
    let shown: Vec<(Option<String>, Vec<String>)> = sections
        .iter()
        .map(|section| {
            (
                section.name.clone(),
                section
                    .items
                    .iter()
                    .map(|dir| dir.name().to_string())
                    .collect(),
            )
        })
        .collect();
    assert_eq!(
        shown,
        vec![
            (Some("Personal".into()), vec!["projk".into()]),
            (Some("Clients".into()), vec!["wsx".into()]),
            (Some("Work".into()), vec!["wsx-ui".into()]),
            (None, vec!["research".into()]),
        ]
    );

    // ^ Moving the last chat of a folder out leaves no folder: no file, no section.
    chats_home::set_folder("projk", None).unwrap();
    chats_home::set_folder("projk", None).unwrap(); // again: nothing to remove, no error
    let ordered: Vec<_> = ["projk", "wsx", "research", "wsx-ui"]
        .iter()
        .map(|name| chats::open_chat(name).unwrap())
        .collect();
    let names: Vec<_> = chats_home::sections(ordered, chats_home::folder)
        .into_iter()
        .map(|section| section.name)
        .collect();
    assert_eq!(
        names,
        vec![Some("Clients".into()), Some("Work".into()), None]
    );

    // ^ A bad name, an empty one, an unknown chat and a closed chat are refused and change nothing.
    for bad in ["", "   ", "two\nlines", &"x".repeat(25)] {
        assert!(chats_home::set_folder("wsx", Some(bad)).is_err(), "{bad:?}");
    }
    assert!(chats_home::set_folder("nobody", Some("Work")).is_err());
    chats::close("research").unwrap();
    assert!(chats_home::set_folder("research", Some("Work")).is_err());
    assert_eq!(
        row_folders()
            .into_iter()
            .map(|(_, folder)| folder)
            .collect::<Vec<_>>(),
        vec![None, Some("Clients".into()), Some("Work".into())],
        "projk, wsx, wsx-ui (closed research is not an open chat)"
    );
    let _ = fs::remove_dir_all(&state);
}
