use std::io::Read;
use std::thread;

use tauri::AppHandle;
use tiny_http::{Header, Method, Response, Server};

use crate::commands::ingest_capture;
use crate::urls::{parse_capture_json, parse_capture_query};

/// Loopback port the Brave bridge POSTs to while Play is running.
pub const CAPTURE_PORT: u16 = 17331;

pub fn start(app: AppHandle) {
    thread::spawn(move || {
        let addr = format!("127.0.0.1:{CAPTURE_PORT}");
        let server = match Server::http(&addr) {
            Ok(s) => s,
            Err(e) => {
                eprintln!("play: capture listener {addr}: {e}");
                return;
            }
        };
        for request in server.incoming_requests() {
            handle(&app, request);
        }
    });
}

fn header(name: &str, value: &str) -> Header {
    Header::from_bytes(name.as_bytes(), value.as_bytes()).expect("header")
}

fn cors<R: std::io::Read>(response: Response<R>) -> Response<R> {
    response
        .with_header(header("Access-Control-Allow-Origin", "*"))
        .with_header(header("Access-Control-Allow-Methods", "GET, POST, OPTIONS"))
        .with_header(header("Access-Control-Allow-Headers", "content-type"))
}

fn handle(app: &AppHandle, mut request: tiny_http::Request) {
    let method = request.method().clone();
    let raw = request.url().to_string();
    let path = raw.split('?').next().unwrap_or("/");

    if method == Method::Options {
        let _ = request.respond(cors(Response::empty(204)));
        return;
    }

    if method == Method::Get && (path == "/" || path == "/health") {
        let _ = request.respond(cors(Response::from_string("ok")));
        return;
    }

    if path != "/open" {
        let _ = request.respond(cors(
            Response::from_string("not found").with_status_code(404),
        ));
        return;
    }

    let capture = if method == Method::Get {
        parse_capture_query(raw.split_once('?').map(|x| x.1).unwrap_or(""))
    } else if method == Method::Post {
        let mut body = String::new();
        let _ = request.as_reader().read_to_string(&mut body);
        parse_capture_json(&body).or_else(|| parse_capture_query(&body))
    } else {
        None
    };

    match capture {
        Some(c) => {
            ingest_capture(app, c);
            let _ = request.respond(cors(
                Response::from_string(r#"{"ok":true}"#)
                    .with_header(header("Content-Type", "application/json")),
            ));
        }
        None => {
            let _ = request.respond(cors(
                Response::from_string(r#"{"ok":false}"#).with_status_code(400),
            ));
        }
    }
}
