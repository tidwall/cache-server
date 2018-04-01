// Copyright 2018 Joshua J Baker. All rights reserved.
// Use of this source code is governed by an MIT-style
// license that can be found in the LICENSE file.

extern crate crossbeam;
extern crate mio;
extern crate num_cpus;
extern crate clap;
extern crate glob;

use std::io;
use std::io::{Read, Write};
use mio::*;
use mio::net::{TcpListener, TcpStream};
use std::collections::HashMap;
use std::sync::Mutex;
use std::sync::Arc;
use std::net::SocketAddr;
use clap::{App, Arg};
use glob::Pattern;

struct Store {
    keys: HashMap<Vec<u8>, Vec<u8>>,
}

impl Store {
    pub fn new() -> Store {
        Store { keys: HashMap::new() }
    }
}

struct Conn {
    stream: TcpStream,
    addr: SocketAddr,
    input: Vec<u8>,
    output: Vec<u8>,
    close: bool,
    reg_write: bool,
}

fn main() {
    let matches = App::new("cache-server")
        .version("v0.0.1")
        .author("Josh Baker <joshbaker77@gmail.com>")
        .arg(
            Arg::with_name("threads")
                .help("Sets the number of threads")
                .short("t")
                .long("threads")
                .takes_value(true),
        )
        .arg(
            Arg::with_name("port")
                .help("Sets the listening port")
                .short("p")
                .long("port")
                .default_value("6380")
                .takes_value(true),
        )
        .get_matches();

    let mut threads = matches
        .value_of("threads")
        .unwrap_or(&num_cpus::get().to_string())
        .parse::<usize>()
        .unwrap();
    if threads == 0 { threads = 1 }

    let port = matches
        .value_of("port")
        .unwrap_or("6380")
        .parse::<usize>()
        .unwrap();

    let addr = format!("0.0.0.0:{}", port);
    let server = TcpListener::bind(&addr.parse().unwrap()).unwrap();
    let main_poll = Poll::new().unwrap();
    main_poll
        .register(&server, Token(0), Ready::readable(), PollOpt::empty())
        .unwrap();
    println!(
        "Server started on port {} using {} thread{}",
        port,
        threads,
        if threads == 1 { "" } else { "s" }
    );

    let main_conns = Arc::new(Mutex::new(HashMap::new()));
    let store = Arc::new(Mutex::new(Store::new()));

    let mut child_polls = Vec::new();
    for _ in 0..threads {
        let poll = Poll::new().unwrap();
        child_polls.push(poll)
    }

    crossbeam::scope(|scope| {
        for poll in &child_polls {
            let mut main_conns = main_conns.clone();
            let mut store = store.clone();
            scope.spawn(move || child_loop(poll, main_conns, store));
        }
        main_loop(&main_poll, &child_polls, main_conns, server)
    });
}

fn main_loop(
    main_poll: &Poll,
    child_polls: &Vec<Poll>,
    main_conns: Arc<Mutex<HashMap<usize, Conn>>>,
    server: TcpListener,
) {
    let mut id = 0;
    let mut events = Events::with_capacity(1);
    loop {
        main_poll.poll(&mut events, None).unwrap();
        events.iter().last().unwrap();
        match server.accept() {
            Ok(s) => {
                s.0
                    .set_keepalive(Some(std::time::Duration::from_secs(300)))
                    .unwrap();
                id += 1;
                let child = &child_polls[id % child_polls.len()];
                child
                    .register(
                        &s.0,
                        Token(id),
                        Ready::readable() | Ready::writable(),
                        PollOpt::empty(),
                    )
                    .unwrap();
                main_conns.lock().unwrap().insert(
                    id,
                    Conn {
                        stream: s.0,
                        addr: s.1,
                        close: false,
                        reg_write: false,
                        input: Vec::new(),
                        output: Vec::new(),
                    },
                );
            }
            Err(ref e) if e.kind() == io::ErrorKind::WouldBlock => {}
            Err(e) => panic!("encountered IO error: {}", e),
        }
    }
}

fn child_loop(
    child_poll: &Poll,
    main_conns: Arc<Mutex<HashMap<usize, Conn>>>,
    store: Arc<Mutex<Store>>,
) {
    let mut packet = [0; 4096];
    let mut streams: HashMap<usize, Conn> = HashMap::new();
    let mut events = Events::with_capacity(1);
    loop {
        child_poll.poll(&mut events, None).unwrap();
        let event = events.iter().last().unwrap();
        let id = event.token().0;
        let mut close = false;
        let mut found = false;
        if let Some(conn) = streams.get_mut(&id) {
            found = true;
            loop {
                if conn.output.len() > 0 {
                    match (&conn.stream).write(conn.output.as_slice()) {
                        Ok(n) => {
                            let mut output = Vec::new();
                            if n < conn.output.len() {
                                output.extend_from_slice(&conn.output[n..conn.output.len()]);
                            }
                            conn.output = output
                        }
                        Err(ref e) if e.kind() == io::ErrorKind::WouldBlock => {}
                        Err(_) => {
                            close = true;
                        }
                    }
                } else if !conn.close {
                    match (&conn.stream).read(&mut packet) {
                        Ok(n) => {
                            if n == 0 {
                                close = true;
                            } else {
                                conn.input.extend_from_slice(&packet[0..n]);
                                let (output, close) = event_data(id, &mut conn.input, &store);
                                conn.output.extend(output);
                                conn.close = close;
                                if conn.output.len() > 0 {
                                    continue;
                                }
                            }
                        }
                        Err(ref e) if e.kind() == io::ErrorKind::WouldBlock => {}
                        Err(_) => {
                            close = true;
                        }
                    }
                }
                break;
            }
            if conn.output.len() > 0 {
                if !conn.reg_write {
                    conn.reg_write = true;
                    child_poll
                        .reregister(
                            &conn.stream,
                            Token(id),
                            Ready::readable() | Ready::writable(),
                            PollOpt::empty(),
                        )
                        .unwrap()
                }
            } else {
                if conn.reg_write {
                    conn.reg_write = false;
                    child_poll
                        .reregister(&conn.stream, Token(id), Ready::readable(), PollOpt::empty())
                        .unwrap()
                }
                if !close {
                    close = conn.close
                }
            }
        }
        if close {
            streams.remove(&id);
            event_closed(id);
        } else if !found {
            if let Some(mut conn) = main_conns.lock().unwrap().remove(&id) {
                let (output, close) = event_opened(id, conn.addr);
                if output.len() > 0 {
                    conn.reg_write = true;
                    conn.close = close;
                    conn.output = output;
                    child_poll
                        .reregister(
                            &conn.stream,
                            Token(id),
                            Ready::writable() | Ready::readable(),
                            PollOpt::empty(),
                        )
                        .unwrap();
                    streams.insert(id, conn);
                } else if !close {
                    child_poll
                        .reregister(&conn.stream, Token(id), Ready::readable(), PollOpt::empty())
                        .unwrap();
                    streams.insert(id, conn);
                }
            }
        }
    }
}

fn redcon_take_inline_args(packet: &Vec<u8>, ni: usize) -> (Vec<Vec<u8>>, String, usize, bool) {
    let mut i = ni;
    let mut s = ni;
    let mut args: Vec<Vec<u8>> = Vec::new();
    while i < packet.len() {
        if packet[i] == b' ' || packet[i] == b'\n' {
            let mut ii = i;
            if packet[i] == b'\n' && i > s && packet[i - 1] == b'\r' {
                ii = i - 1;
            }
            if s != ii {
                args.push(packet[s..ii].to_vec());
            }
            if packet[i] == b'\n' {
                return (args, String::default(), i + 1, true);
            }
            s = i + 1;
        } else if packet[i] == b'"' || packet[i] == b'\'' {
            let mut arg = Vec::new();
            let ch = packet[i];
            i += 1;
            s = i;
            while i < packet.len() {
                if packet[i] == b'\n' {
                    return (
                        Vec::default(),
                        "ERR Protocol error: unbalanced quotes in request".to_string(),
                        ni,
                        false,
                    );
                } else if packet[i] == b'\\' {
                    i += 1;
                    match packet[i] {
                        b'n' => arg.push(b'\n'),
                        b'r' => arg.push(b'\r'),
                        b't' => arg.push(b'\t'),
                        b'b' => arg.push(0x08),
                        b'a' => arg.push(0x07),
                        b'x' => {
                            let is_hex = |b: u8| -> bool {
                                (b >= b'0' && b <= b'9') || (b >= b'a' && b <= b'f') ||
                                    (b >= b'A' && b <= b'F')
                            };
                            let hex_to_digit = |b: u8| -> u8 {
                                if b <= b'9' {
                                    b - b'0'
                                } else if b <= b'F' {
                                    b - b'A' + 10
                                } else {
                                    b - b'a' + 10
                                }
                            };
                            if packet.len() - (i + 1) >= 2 && is_hex(packet[i + 1]) &&
                                is_hex(packet[i + 2])
                            {
                                arg.push(
                                    hex_to_digit(packet[i + 1]) << 4 + hex_to_digit(packet[i + 2]),
                                );
                                i += 2
                            } else {
                                arg.push(b'x')
                            }
                        }
                        _ => arg.push(packet[i]),
                    }
                } else if packet[i] == ch {
                    args.push(arg);
                    s = i + 1;
                    break;
                } else {
                    arg.push(packet[i]);
                }
                i += 1;
            }
        }
        i += 1;
    }
    (Vec::default(), String::default(), ni, false)
}

fn redcon_take_multibulk_args(input: &Vec<u8>, ni: usize) -> (Vec<Vec<u8>>, String, usize, bool) {
    let mut err = String::default();
    let mut complete = false;
    let mut args: Vec<Vec<u8>> = Vec::new();
    let mut i = ni + 1;
    let mut s = ni;
    while i < input.len() {
        if input[i - 1] == b'\r' && input[i] == b'\n' {
            match String::from_utf8_lossy(&input[s + 1..i - 1]).parse::<usize>() {
                Ok(nargs) => {
                    i += 1;
                    complete = nargs == 0;
                    for _ in 0..nargs {
                        s = i;
                        while i < input.len() {
                            if input[i - 1] == b'\r' && input[i] == b'\n' {
                                if input[s] != b'$' {
                                    err = format!("expected '$', got '{}'", input[s] as char);
                                    break;
                                }
                                match String::from_utf8_lossy(&input[s + 1..i - 1])
                                    .parse::<usize>() {
                                    Ok(nbytes) => {
                                        if input.len() < i + 1 + nbytes + 2 {
                                            break;
                                        }
                                        let bin = input[i + 1..i + 1 + nbytes].to_vec();
                                        args.push(bin);
                                        i = i + 1 + nbytes + 2;
                                    }
                                    Err(_) => {
                                        err = "invalid bulk length".to_string();
                                    }
                                }
                                break;
                            }
                            i += 1;
                        }
                        if err != "" {
                            break;
                        }
                        if args.len() == nargs {
                            complete = true;
                            break;
                        }
                    }
                }
                Err(_) => {
                    err = "invalid multibulk length".to_string();
                }
            }
            break;
        }
        i += 1;
    }
    if err != "" {
        err = format!("ERR Protocol error: {}", safe_line_from_string(err))
    }
    (args, err, i, complete)
}

fn redcon_take_args(input: &Vec<u8>, ni: usize) -> (Vec<Vec<u8>>, String, usize, bool) {
    if input.len() > ni {
        if input[ni] == b'*' {
            redcon_take_multibulk_args(input, ni)
        } else {
            redcon_take_inline_args(input, ni)
        }
    } else {
        (Vec::default(), String::default(), ni, false)
    }
}

fn safe_line_from_string(s: String) -> String {
    safe_line_from_slice(s.as_bytes())
}

fn safe_line_from_slice(s: &[u8]) -> String {
    let mut out = Vec::new();
    for i in 0..s.len() {
        if s[i] < b' ' {
            out.push(b' ')
        } else {
            out.push(s[i]);
        }
    }
    String::from_utf8_lossy(out.as_slice()).to_string()
}

fn arg_match(arg: &[u8], what: &str) -> bool {
    if arg.len() != what.len() {
        return false;
    }
    let what = what.as_bytes();
    for i in 0..arg.len() {
        if arg[i] != what[i] {
            if arg[i] >= b'a' && arg[i] <= b'z' {
                if arg[i] != what[i] + 32 {
                    return false;
                }
            } else if arg[i] >= b'A' && arg[i] <= b'Z' {
                if arg[i] != what[i] - 32 {
                    return false;
                }
            }
        }
    }
    return true;
}

fn event_opened(_id: usize, _addr: SocketAddr) -> (Vec<u8>, bool) {
    // FUTURE: Hola connection.
    (Vec::new(), false)
}

fn event_closed(_id: usize) {
    // FUTURE: Adios connection.
}

fn event_data(_id: usize, input: &mut Vec<u8>, store: &Arc<Mutex<Store>>) -> (Vec<u8>, bool) {
    let mut output = Vec::new();
    let mut close = false;
    let mut i = 0;
    let mut argss = Vec::new();
    loop {
        let (args, err, ni, complete) = redcon_take_args(input, i);
        if err != "" {
            output.extend(format!("-{}\r\n", err).into_bytes());
            close = true;
            break;
        } else if !complete {
            break;
        }
        i = ni;
        if args.len() > 0 {
            argss.push(args);
        }
    }

    if !close && argss.len() > 0 {
        //let mut aof = Vec::new();
        let mut store = store.lock().unwrap();
        for args in argss {
            let (hout, write, hclose) = handle_command(&args, &mut store.keys);
            output.extend_from_slice(hout.as_slice());
            if hclose {
                close = true;
                break;
            }
            if write {
                //aof.extend(hout);
            }
        }
        // if aof.len() > 0 {
        //     // FUTURE: persist to disk
        // }
    }
    if i > 0 {
        if i < input.len() {
            let mut remain = Vec::new();
            remain.extend_from_slice(&input[i..input.len()]);
            input.clear();
            input.extend(remain)
        } else {
            input.clear()
        }
    }
    (output, close)
}

fn make_bulk(bulk: &Vec<u8>) -> Vec<u8> {
    let mut resp = Vec::new();
    resp.push(b'$');
    resp.extend_from_slice(&bulk.len().to_string().into_bytes());
    resp.push(b'\r');
    resp.push(b'\n');
    resp.extend(bulk);
    resp.push(b'\r');
    resp.push(b'\n');
    resp
}

fn make_array(count: usize) -> Vec<u8> {
    let mut resp = Vec::new();
    resp.push(b'*');
    resp.extend_from_slice(&count.to_string().into_bytes());
    resp.push(b'\r');
    resp.push(b'\n');
    resp
}

fn invalid_num_args(cmd: &Vec<u8>) -> Vec<u8> {
    format!(
        "-ERR wrong number of arguments for '{}' command\r\n",
        String::from_utf8_lossy(cmd.as_slice())
    ).into_bytes()
        .to_vec()
}

fn handle_command(
    args: &Vec<Vec<u8>>,
    keys: &mut HashMap<Vec<u8>, Vec<u8>>,
) -> (Vec<u8>, bool, bool) {
    if arg_match(&args[0], "PING") {
        match args.len() {
            1 => (b"+PONG\r\n".to_vec(), false, false),
            2 => (make_bulk(&args[1]), false, false),
            _ => (invalid_num_args(&args[0]), false, false),
        }
    } else if arg_match(&args[0], "SET") {
        match args.len() {
            3 => {
                keys.insert(args[1].clone(), args[2].clone());
                (b"+OK\r\n".to_vec(), true, false)
            }
            _ => (invalid_num_args(&args[0]), false, false),
        }
    } else if arg_match(&args[0], "FLUSHDB") {
        match args.len() {
            1 => {
                keys.clear();
                (b"+OK\r\n".to_vec(), true, false)
            }
            _ => (invalid_num_args(&args[0]), false, false),
        }
    } else if arg_match(&args[0], "DEL") {
        match args.len() {
            2 => {
                if let Some(_) = keys.remove(&args[1]) {
                    (b":1\r\n".to_vec(), true, false)
                } else {
                    (b":0\r\n".to_vec(), false, false)
                }
            }
            _ => (invalid_num_args(&args[0]), false, false),
        }
    } else if arg_match(&args[0], "GET") {
        match args.len() {
            2 => {
                match keys.get(&args[1]) {
                    Some(v) => (make_bulk(v), false, false),
                    None => (b"$-1\r\n".to_vec(), false, false),
                }
            }
            _ => (invalid_num_args(&args[0]), false, false),
        }
    } else if arg_match(&args[0], "KEYS") {
        match args.len() {
            2 => {
                match Pattern::new(&String::from_utf8_lossy(args[1].as_slice()).clone()) {
                    Ok(pat) => {
                        let mut res_keys = Vec::new();
                        for (key, _val) in keys.iter() {
                            if pat.matches(&String::from_utf8_lossy(key)) {
                                res_keys.push(key);
                            }
                        }
                        let mut output = make_array(res_keys.len());
                        for key in res_keys {
                            output.extend(make_bulk(key));
                        }
                        (output, false, false)
                    }
                    Err(_) => (b"$-1\r\n".to_vec(), false, false),
                }
            }
            _ => (invalid_num_args(&args[0]), false, false),
        }
    } else if arg_match(&args[0], "QUIT") {
        (b"+OK\r\n".to_vec(), false, true)
    } else {
        (
            format!(
                "-ERR unknown command '{}'\r\n",
                safe_line_from_slice(&args[0])
            ).into_bytes()
                .to_vec(),
            false,
            false,
        )
    }
}
