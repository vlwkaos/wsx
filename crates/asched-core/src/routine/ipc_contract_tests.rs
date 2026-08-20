use super::*;
use crate::routine::RoutineFire;
use std::io::Cursor;

fn preserve_io(error: std::io::Error) -> RoutineError {
    error.into()
}

#[test]
fn given_newline_terminated_frame_at_maximum_when_read_then_payload_is_accepted() {
    let result = read_response_frame(Cursor::new(b"abc\n"), 3, preserve_io);

    assert_eq!(result.unwrap(), b"abc");
}

#[test]
fn given_frame_without_newline_when_read_then_corrupt_is_reported() {
    let result = read_response_frame(Cursor::new(b"abc"), 3, preserve_io);

    assert!(matches!(result, Err(RoutineError::Corrupt(_))));
}

#[test]
fn given_newline_terminated_frame_over_maximum_when_read_then_corrupt_is_reported() {
    let result = read_response_frame(Cursor::new(b"abcd\n"), 3, preserve_io);

    assert!(matches!(result, Err(RoutineError::Corrupt(_))));
}

#[test]
fn given_fire_request_and_response_when_serialized_then_contract_round_trips() {
    let request = Request::new(
        "/project".into(),
        Action::Fire {
            kind: "filesystem.changed".into(),
            payload: serde_json::json!({"path": "src/main.rs"}),
            event_id: "delivery-1".into(),
        },
    );
    let response = Response::Fire {
        outcome: FireOutcome::Handled {
            routines: vec![RoutineFire::Started {
                name: "check".into(),
            }],
        },
    };

    let decoded_request: Request =
        serde_json::from_slice(&serde_json::to_vec(&request).unwrap()).unwrap();
    let decoded_response: Response =
        serde_json::from_slice(&serde_json::to_vec(&response).unwrap()).unwrap();

    assert!(matches!(
        decoded_request.action,
        Action::Fire { kind, event_id, .. }
            if kind == "filesystem.changed" && event_id == "delivery-1"
    ));
    assert!(matches!(
        decoded_response,
        Response::Fire {
            outcome: FireOutcome::Handled { routines }
        } if routines == vec![RoutineFire::Started { name: "check".into() }]
    ));
}
