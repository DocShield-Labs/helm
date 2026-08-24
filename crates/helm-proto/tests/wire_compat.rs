use helm_proto::{
    encode_frame, BlockId, BlockMeta, ClientMsg, Cursor, CursorShape, DaemonMsg, Notification,
    NotificationId, NotificationKind, PathCompletion, PathEntryKind, Row, Screen, SearchMatch,
    SearchScope, SessionId, TreeSnapshot, COMPATIBILITY_BASELINE, PROTOCOL_VERSION,
};
use serde::{Deserialize, Serialize};

fn body<T: Serialize>(message: &T) -> Vec<u8> {
    encode_frame(message).unwrap()[4..].to_vec()
}

fn variant_index<T: Serialize>(message: &T) -> u32 {
    let bytes = body(message);
    u32::from_le_bytes(bytes[..4].try_into().unwrap())
}

#[test]
fn protocol_seven_is_the_permanent_compatibility_baseline() {
    assert_eq!(PROTOCOL_VERSION, 7);
    assert_eq!(COMPATIBILITY_BASELINE, 7);
}

#[test]
fn client_variant_indices_are_frozen() {
    let cases = [
        (
            ClientMsg::Hello {
                protocol_version: 7,
                client_name: String::new(),
            },
            0,
        ),
        (ClientMsg::Attach, 1),
        (
            ClientMsg::Input {
                session: SessionId(1),
                bytes: vec![],
            },
            2,
        ),
        (
            ClientMsg::Resize {
                session: SessionId(1),
                cols: 1,
                rows: 1,
            },
            3,
        ),
        (
            ClientMsg::Screen {
                req_id: 1,
                session: SessionId(1),
            },
            4,
        ),
        (
            ClientMsg::History {
                req_id: 1,
                session: SessionId(1),
                from_line: 0,
                to_line: 1,
            },
            5,
        ),
        (
            ClientMsg::NewSession {
                req_id: 1,
                name: None,
                cwd: None,
                command: None,
            },
            6,
        ),
        (
            ClientMsg::KillSession {
                session: SessionId(1),
            },
            7,
        ),
        (
            ClientMsg::RenameSession {
                session: SessionId(1),
                name: String::new(),
            },
            8,
        ),
        (
            ClientMsg::Search {
                req_id: 1,
                query: String::new(),
                regex: false,
                case_sensitive: false,
                scope: SearchScope::All,
                max_results: 1,
            },
            9,
        ),
        (
            ClientMsg::Blocks {
                req_id: 1,
                session: SessionId(1),
            },
            10,
        ),
        (
            ClientMsg::CompletePath {
                req_id: 1,
                session: SessionId(1),
                path: String::new(),
                directories_only: false,
                max_results: 1,
            },
            11,
        ),
        (ClientMsg::Ping { req_id: 1 }, 12),
        (
            ClientMsg::AckNotifications {
                up_to: NotificationId(1),
            },
            13,
        ),
        (ClientMsg::Shutdown, 14),
        (
            ClientMsg::Extension {
                req_id: None,
                name: String::new(),
                payload: vec![],
            },
            15,
        ),
    ];
    for (message, expected) in cases {
        assert_eq!(variant_index(&message), expected, "{message:?}");
    }
}

#[test]
fn daemon_variant_indices_are_frozen() {
    let cursor = Cursor {
        row: 0,
        col: 0,
        visible: true,
        shape: CursorShape::Block,
        blink: false,
    };
    let screen = Screen {
        cols: 1,
        rows: 1,
        top_line: 0,
        history_start: 0,
        lines: vec![Row::default()],
        cursor,
        modes: 0,
    };
    let block = BlockMeta {
        id: BlockId(1),
        start_line: 0,
        cmd_line: None,
        output_line: None,
        end_line: None,
        cmdline: None,
        cwd: None,
        branch: None,
        root: None,
        exit_code: None,
        started_at_ms: None,
        finished_at_ms: None,
    };
    let cases = [
        (
            DaemonMsg::HelloAck {
                protocol_version: 7,
                daemon_version: String::new(),
                state: TreeSnapshot { sessions: vec![] },
                pending: vec![],
            },
            0,
        ),
        (
            DaemonMsg::Screen {
                req_id: None,
                session: SessionId(1),
                screen,
            },
            1,
        ),
        (
            DaemonMsg::ScreenDiff {
                session: SessionId(1),
                top_line: 0,
                scroll: 0,
                rows: vec![],
                cursor,
                modes: 0,
            },
            2,
        ),
        (
            DaemonMsg::HistoryAppend {
                session: SessionId(1),
                first_line: 0,
                rows: vec![],
            },
            3,
        ),
        (
            DaemonMsg::History {
                req_id: 1,
                session: SessionId(1),
                from_line: 0,
                rows: vec![],
                history_start: 0,
                top_line: 0,
            },
            4,
        ),
        (
            DaemonMsg::Block {
                session: SessionId(1),
                block: block.clone(),
            },
            5,
        ),
        (
            DaemonMsg::ModeChange {
                session: SessionId(1),
                alt_screen: false,
            },
            6,
        ),
        (
            DaemonMsg::TreeChanged {
                state: TreeSnapshot { sessions: vec![] },
            },
            7,
        ),
        (
            DaemonMsg::SessionExited {
                session: SessionId(1),
                status: None,
            },
            8,
        ),
        (
            DaemonMsg::Created {
                req_id: 1,
                session: SessionId(1),
            },
            9,
        ),
        (
            DaemonMsg::SearchResults {
                req_id: 1,
                matches: vec![SearchMatch {
                    session: SessionId(1),
                    block: None,
                    line: 0,
                    line_text: String::new(),
                    match_start: 0,
                    match_end: 0,
                }],
                truncated: false,
            },
            10,
        ),
        (
            DaemonMsg::Blocks {
                req_id: 1,
                session: SessionId(1),
                blocks: vec![block],
            },
            11,
        ),
        (
            DaemonMsg::PathCompletions {
                req_id: 1,
                session: SessionId(1),
                candidates: vec![PathCompletion {
                    value: String::new(),
                    kind: PathEntryKind::File,
                }],
                truncated: false,
            },
            12,
        ),
        (DaemonMsg::Pong { req_id: 1 }, 13),
        (
            DaemonMsg::Notification {
                note: Notification {
                    id: NotificationId(1),
                    session: SessionId(1),
                    kind: NotificationKind::Bell,
                    preview: String::new(),
                    at_ms: 0,
                },
            },
            14,
        ),
        (
            DaemonMsg::Error {
                req_id: None,
                context: String::new(),
                message: String::new(),
            },
            15,
        ),
        (
            DaemonMsg::Capabilities {
                compatibility_baseline: 7,
                extensions: vec![],
            },
            16,
        ),
        (
            DaemonMsg::Extension {
                req_id: None,
                name: String::new(),
                payload: vec![],
            },
            17,
        ),
    ];
    for (message, expected) in cases {
        assert_eq!(variant_index(&message), expected, "{message:?}");
    }
}

#[test]
fn protocol_seven_hello_has_a_golden_encoding() {
    assert_eq!(
        body(&ClientMsg::Hello {
            protocol_version: 7,
            client_name: "helm-app".into(),
        }),
        [
            0, 0, 0, 0, 7, 0, 0, 0, 8, 0, 0, 0, 0, 0, 0, 0, b'h', b'e', b'l', b'm', b'-', b'a',
            b'p', b'p',
        ]
    );

    assert_eq!(
        body(&DaemonMsg::HelloAck {
            protocol_version: 7,
            daemon_version: "x".into(),
            state: TreeSnapshot { sessions: vec![] },
            pending: vec![],
        }),
        [
            0, 0, 0, 0, 7, 0, 0, 0, 1, 0, 0, 0, 0, 0, 0, 0, b'x', 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
            0, 0, 0, 0, 0,
        ]
    );
}

#[allow(dead_code)]
#[derive(Serialize, Deserialize)]
enum FutureClientMsg {
    Hello {
        protocol_version: u32,
        client_name: String,
    },
    V1,
    V2,
    V3,
    V4,
    V5,
    V6,
    V7,
    V8,
    V9,
    V10,
    V11,
    Ping {
        req_id: u64,
    },
    V13,
    V14,
    V15,
    ProtocolEightFeature,
    ProtocolNineFeature,
}

#[allow(dead_code)]
#[derive(Serialize, Deserialize)]
enum FutureDaemonMsg {
    HelloAck {
        protocol_version: u32,
        daemon_version: String,
        state: TreeSnapshot,
        pending: Vec<Notification>,
    },
    V1,
    V2,
    V3,
    V4,
    V5,
    V6,
    V7,
    V8,
    V9,
    V10,
    V11,
    V12,
    Pong {
        req_id: u64,
    },
    V14,
    V15,
    V16,
    V17,
    ProtocolEightFeature,
    ProtocolNineFeature,
}

#[test]
fn protocol_nine_can_reuse_protocol_seven_common_messages() {
    let future_hello = FutureClientMsg::Hello {
        protocol_version: 7,
        client_name: "helm-app 0.9.0".into(),
    };
    assert!(matches!(
        bincode::deserialize::<ClientMsg>(&body(&future_hello)).unwrap(),
        ClientMsg::Hello {
            protocol_version: 7,
            ..
        }
    ));

    let future_ping = FutureClientMsg::Ping { req_id: 99 };
    assert!(matches!(
        bincode::deserialize::<ClientMsg>(&body(&future_ping)).unwrap(),
        ClientMsg::Ping { req_id: 99 }
    ));

    let future_ack = FutureDaemonMsg::HelloAck {
        protocol_version: 7,
        daemon_version: "0.2.7".into(),
        state: TreeSnapshot { sessions: vec![] },
        pending: vec![],
    };
    assert!(matches!(
        bincode::deserialize::<DaemonMsg>(&body(&future_ack)).unwrap(),
        DaemonMsg::HelloAck {
            protocol_version: 7,
            ..
        }
    ));

    let future_pong = FutureDaemonMsg::Pong { req_id: 99 };
    assert!(matches!(
        bincode::deserialize::<DaemonMsg>(&body(&future_pong)).unwrap(),
        DaemonMsg::Pong { req_id: 99 }
    ));
}
