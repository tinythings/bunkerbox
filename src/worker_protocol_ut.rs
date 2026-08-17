use super::*;

fn ids() -> (WorkerRequestId, WorkerSessionId, WorkerUploadId) {
    (WorkerRequestId([0x11; 16]), WorkerSessionId([0x22; 16]), WorkerUploadId([0x33; 16]))
}

#[test]
fn ticket_11_v1_frame_header_and_payload_bytes_remain_stable() {
    let (request_id, session_id, _) = ids();
    let message = WorkerMessage::Hello { request_id, session_id, version: WORKER_PROTOCOL_VERSION, response: false };
    let expected = [
        b'B',
        b'B',
        b'W',
        b'K',
        1,
        0,
        WorkerFrameKind::Hello as u8,
        0x23,
        0,
        0,
        0,
        0x11,
        0x11,
        0x11,
        0x11,
        0x11,
        0x11,
        0x11,
        0x11,
        0x11,
        0x11,
        0x11,
        0x11,
        0x11,
        0x11,
        0x11,
        0x11,
        0x22,
        0x22,
        0x22,
        0x22,
        0x22,
        0x22,
        0x22,
        0x22,
        0x22,
        0x22,
        0x22,
        0x22,
        0x22,
        0x22,
        0x22,
        0x22,
        1,
        0,
        0,
    ];
    assert_eq!(message.encode().unwrap(), expected);
}

#[test]
fn host_reexport_decodes_ticket_11_v1_bytes() {
    let (request_id, session_id, _) = ids();
    let message = WorkerMessage::hello(request_id, session_id, false);
    let frame = message.encode().unwrap();
    assert_eq!(WorkerMessage::decode(&frame).unwrap(), message);
}
