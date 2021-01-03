mod rendering;
mod http;

use std::net::TcpListener;
use std::net::TcpStream;

use std::cmp::min;

use std::io;
use std::io::{Read,Seek,SeekFrom};

use std::str::from_utf8;

use std::fs;

use std::format;

use std::collections::HashMap;

use nix::sys::select::{select,FdSet};
use std::os::unix::io::AsRawFd;
use std::os::unix::prelude::RawFd;

use std::path::Path;

use http::{
    HttpRequest, HttpResponse, HttpStatus,
    HttpVersion, HttpMethod
};

const BUFFER_SIZE: usize = 4096;

fn resolve_io_error(error: &io::Error) -> Option<HttpStatus> {
    match error.kind() {
        io::ErrorKind::NotFound => Some(HttpStatus::NotFound),
        io::ErrorKind::PermissionDenied => Some(HttpStatus::PermissionDenied),
        _ => None
    }
}

struct ContentRange {
    pub start: usize,
    pub len: Option<usize>,
}

fn decode_content_range(range_str: &str) -> Option<ContentRange> {
    if !range_str.starts_with("bytes=") {
        return None;
    }
    let eq_ind = match range_str.find('=') {
        Some(i) => i,
        _ => { return None; }
    };
    let dash_ind = match range_str.find('-') {
        Some(i) => i,
        _ => { return None; }
    };

    let start_str = &range_str[eq_ind + 1..dash_ind];
    let end_str = &range_str[dash_ind + 1..];

    let start_int: usize = if start_str.len() > 0 {
        match start_str.parse() {
            Ok(i) => i,
            _ => { return None; }
        }
    } else {
        0
    };

    let end_int: Option<usize> = if end_str.len() > 0 {
        match end_str.parse() {
            Ok(i) => Some(i),
            _ => { return None; }
        }
    } else {
        None
    };

    if let Some(end_i) = end_int {
        if end_i == 0 || start_int > end_i {
            None
        } else {
            Some(ContentRange {
                start: start_int,
                len: Some(end_i - start_int)
            })
        }
    } else {
        Some(ContentRange {
            start: start_int,
            len: None,
        })
    }
}

fn decode_request(req_body: &[u8]) -> Result<HttpRequest, HttpStatus> {
    let request_str = match from_utf8(req_body) {
        Ok(dec) => dec,
        Err(_err) => {
            // write_error(format!("Could not decode request: {}", err));
            return Err(HttpStatus::BadRequest);
        }
    };

    return HttpRequest::new(request_str);
}

fn end_of_http_request(req_body: &[u8]) -> bool {
    if req_body.len() < 4 { return false; }
    return &req_body[req_body.len() - 4 ..] == b"\r\n\r\n";
}

#[derive(PartialEq, Debug)]
pub enum ConnectionState {
    ReadingRequest,
    WritingResponse,
    Closing
}

pub struct SeekableString {
    pub start: usize,
    pub data: String,
}

impl SeekableString {
    pub fn new(d: String) -> SeekableString {
        SeekableString {
            start: 0,
            data: d
        }
    }
}

impl Read for SeekableString {
    fn read(&mut self, buf: &mut [u8]) -> Result<usize, io::Error> {
        let mut slice = &self.data.as_bytes()[self.start..];
        let read = slice.read(buf)?;
        self.start += read;
        Ok(read)
    }
}

impl Seek for SeekableString {
    fn seek(&mut self, pos: SeekFrom) -> Result<u64, io::Error>{
        self.start = match pos {
            SeekFrom::Start(i) => i as usize,
            SeekFrom::Current(i) => ((self.start as i64) + i) as usize,
            SeekFrom::End(i) => ((self.data.len() as i64) - i) as usize,
        };
        Ok(self.start as u64)
    }
}

pub enum ResponseDataType {
    None,
    StringSegment(SeekableString),
    FileSegment(fs::File),
}

pub struct HttpConnection {
    pub stream: TcpStream,
    pub state: ConnectionState,

    // Buffer for holding a pending request
    pub buffer: [u8; BUFFER_SIZE],
    pub bytes_read: usize,

    // Space to store a per-request string response
    pub response_data: ResponseDataType,
    pub response: Option<HttpResponse>,

    pub last_requested_uri: Option<String>,

    pub keep_alive: bool,

    pub bytes_requested: usize,
    pub bytes_sent: usize,
}

impl HttpConnection {
    pub fn new(stream: TcpStream) -> HttpConnection {
        return HttpConnection {
            stream: stream,
            state: ConnectionState::ReadingRequest,
            buffer: [0; BUFFER_SIZE],
            bytes_read: 0,
            response_data: ResponseDataType::None,
            response: None,
            keep_alive: true,
            bytes_requested: 0,
            bytes_sent: 0,
            last_requested_uri: None,
        }
    }

    pub fn reset(&mut self) {
        self.bytes_read = 0;
        self.response = None;
    }
}

pub struct HttpTui<'a> {
    listener: TcpListener,
    root_dir: &'a Path,
}

impl HttpTui<'_> {
    pub fn new<'a>(host: &str, port: u16, root_dir: &'a Path) -> Result<HttpTui<'a>, io::Error> {
        let listener = TcpListener::bind(format!("{mask}:{port}", mask=host, port=port))?;
        Ok(HttpTui {
            listener: listener,
            root_dir: root_dir,
        })
    }

    pub fn run(&mut self, pipe_read: RawFd, func: impl Fn(&HashMap::<RawFd, HttpConnection>)) {
        let mut connections = HashMap::<RawFd, HttpConnection>::new();
        let l_raw_fd = self.listener.as_raw_fd();

        'main: loop {
            let mut r_fds = FdSet::new();
            let mut w_fds = FdSet::new();
            let mut e_fds = FdSet::new();

            // First add listener:
            r_fds.insert(l_raw_fd);
            e_fds.insert(l_raw_fd);

            r_fds.insert(pipe_read);
            e_fds.insert(pipe_read);

            for (fd, http_conn) in &connections {
                match http_conn.state {
                    ConnectionState::WritingResponse => { w_fds.insert(*fd); }
                    ConnectionState::ReadingRequest  => { r_fds.insert(*fd); }
                    _ => {}
                }
                e_fds.insert(*fd);
            }

            match select(None, Some(&mut r_fds), Some(&mut w_fds), Some(&mut e_fds), None) {
                Ok(_res) => {},
                Err(e) => {
                    println!("Got error while selecting: {}", e);
                    break;
                }
            }

            match r_fds.highest() {
                None => {},
                Some(mfd) => {
                    for fd in 0..(mfd + 1) {
                        if !r_fds.contains(fd) { continue; }
                        // if !connections.contains_key(&fd) { continue; }

                        // If we have data to read on the pipe, we need to close
                        if fd == pipe_read {
                            break 'main;
                        }
                        // If listener, get accept new connection and add it.
                        if fd == l_raw_fd {
                            match self.listener.accept() {
                                Ok((stream, _addr)) => {
                                    let conn = HttpTui::create_http_connection(stream);
                                    connections.insert(conn.stream.as_raw_fd(), conn);
                                }
                                _ => {},
                                // Err(e) => write_error(e.to_string()),
                            }
                        } else {
                            assert_eq!(connections[&fd].state, ConnectionState::ReadingRequest);
                            // TODO: Error checking here
                            let mut conn = connections.get_mut(&fd).unwrap();
                            match self.handle_conn_sigpipe(&mut conn) {
                                Ok(_) => {},
                                Err(error) => {
                                    let _ = self.write_error_response(HttpStatus::ServerError, conn, Some(&error.to_string()));
                                    // write_error(format!("Server error while reading: {}", error));
                                }
                            };
                            if connections[&fd].state == ConnectionState::Closing {
                                // Delete to close connection
                                connections.remove(&fd);
                            }
                        }
                    }
                }
            }
            match w_fds.highest() {
                None => {},
                Some(mfd) => {
                    for fd in 0..(mfd + 1) {
                        if !w_fds.contains(fd) { continue; }
                        // if !connections.contains_key(&fd) { continue; }
                        assert_eq!(connections[&fd].state, ConnectionState::WritingResponse);
                        match self.handle_conn_sigpipe(&mut connections.get_mut(&fd).unwrap()) {
                            Ok(_) => {},
                            _ => {},
                            // Err(error) => { write_error(format!("Server error while writing: {}", error)); }
                        }
                        if connections[&fd].state == ConnectionState::Closing {
                            // Delete to close connection
                            connections.remove(&fd);
                        }
                    }
                }
            }
            match e_fds.highest() {
                None => {},
                Some(mfd) => {
                    for fd in 0..(mfd + 1) {
                        if !e_fds.contains(fd) { continue; }
                        // if !connections.contains_key(&fd) { continue; }
                        if fd == pipe_read {
                            break 'main;
                        }
                        // If listener, get accept new connection and add it.
                        if fd == l_raw_fd {
                            eprintln!("Listener socket has errored!");
                            break 'main;
                        } else {
                            // Ignore the return value of write_error_response, because
                            // we're closing the connection anyway.
                            let _ = self.write_error_response(HttpStatus::ServerError, connections.get_mut(&fd).unwrap(), Some("Peer socket in error state"));
                            println!("Got bad state on client socket");
                            connections.remove(&fd);
                        }
                    }
                }
            }

            func(&connections);
        }
    }

    fn read_partial_request(&self, conn: &mut HttpConnection) -> Result<ConnectionState, io::Error> {
        let buffer = &mut conn.buffer;
        let bytes_read = match conn.stream.read(&mut buffer[conn.bytes_read..]) {
            Ok(size) => size,
            Err(_err) => {
                /*
                write_error(format!(
                    "Failed to read bytes from socket: {}", err));
                */
                // Even though the server has run into a problem, because it is
                // a problem inherent to the socket connection, we return Ok
                // so that we do not write an HTTP error response to the socket.
                return Ok(ConnectionState::Closing);
            }
        };

        conn.bytes_read += bytes_read;
        if bytes_read == 0 || end_of_http_request(&buffer[..conn.bytes_read]) {
            // Once we have read the request, handle it.
            // The connection state will be updated accordingly
            self.handle_request(conn)
        } else {
            Ok(ConnectionState::ReadingRequest)
        }
    }

    fn handle_request(&self, mut conn: &mut HttpConnection) -> Result<ConnectionState, io::Error> {
        let body = &mut conn.buffer[..conn.bytes_read];

        let req: HttpRequest = match decode_request(body) {
            Ok(r) => r,
            Err(status) => {
                return self.write_error_response(status, conn, None);
            }
        };

        conn.last_requested_uri = Some(req.path.to_string());

        // Check if keep-alive header was given in the request.
        // If it was not, assume keep-alive is >= HTTP/1.1.
        conn.keep_alive = match req.get_header("connection") {
            Some(value) => value.to_lowercase() == "keep-alive",
            None => false,
        };

        if req.method.is_none() {
            return self.write_error_response(HttpStatus::NotImplemented, conn, None);
        }

        let normalized_path = if req.path.starts_with("/") {
            &req.path[1..]
        } else {
            &req.path[..]
        };

        let path = self.root_dir.join(normalized_path);
        let canonical_path = match fs::canonicalize(path) {
            Err(error) => {
                // Attempt to convert the system error into an HTTP error
                // that we can send back to the user.
                return match resolve_io_error(&error) {
                    Some(http_error) => self.write_error_response(http_error, conn, None),
                    None => Err(error),
                };
            }
            Ok(path) => path
        };

        if !canonical_path.starts_with(self.root_dir) {
            // Use 404 so that the user cannot determine if directories
            // exist or not.
            return self.write_error_response(HttpStatus::NotFound, conn, None);
        }

        let metadata = match fs::metadata(&canonical_path) {
            Err(error) => {
                return match resolve_io_error(&error) {
                    Some(http_error) => self.write_error_response(http_error, conn, None),
                    None => Err(error),
                };
            }
            Ok(data) => data
        };

        if !metadata.is_file() && !metadata.is_dir() {
            return self.write_error_response(HttpStatus::PermissionDenied, conn, Some(&format!("Attempted to read an irregular file.")));
        }

        let (mut response_data, full_length, mime) = if metadata.is_dir() {
                let s: String = rendering::render_directory(normalized_path, canonical_path.as_path());
                let len = s.len();
                let data = ResponseDataType::StringSegment(SeekableString::new(s));
                (data, len, Some("text/html"))
            } else {
                let data = ResponseDataType::FileSegment(fs::File::open(&canonical_path)?);
                let len = if metadata.is_file() {
                    metadata.len() as usize
                } else {
                    std::u32::MAX as usize
                };
                // (data, len, None)
                (data, len, if req.path.ends_with(".html") {
                    Some("text/html")
                } else if req.path.ends_with(".mp4") {
                    Some("video/mp4")
                } else {
                    Some("application/octet-stream")
                })
            };

        let (start, range, used_range) = match req.get_header("range") {
            Some(content_range_str) => {
                if let Some(content_range) = decode_content_range(content_range_str) {
                    let real_start = min(content_range.start, full_length);
                    let real_len = match content_range.len {
                        Some(len) => min(len, full_length - real_start),
                        None => full_length - real_start
                    };
                    (real_start, real_len, true)
                } else {
                    return self.write_error_response(HttpStatus::BadRequest, conn, Some(&format!("Could not decode Range header")));
                }
            },
            None => {
                (0, full_length, false)
            }
        };

        let mut resp = HttpResponse::new(if used_range {
                HttpStatus::PartialContent
            } else {
                HttpStatus::OK
            }, &req.version);

        resp.add_header("Server".to_string(), "http-tui".to_string());
        resp.add_header("Accept-Ranges".to_string(), "bytes".to_string());

        resp.set_content_length(range);

        if used_range {
            resp.add_header("Content-Range".to_string(), format!("bytes {}-{}/{}", start, start + range - 1, full_length));
            match response_data {
                ResponseDataType::StringSegment(ref mut seg) => {
                    seg.seek(io::SeekFrom::Start((start) as u64))?;
                },
                ResponseDataType::FileSegment(ref mut file) => {
                    file.seek(io::SeekFrom::Start((start) as u64))?;
                },
                _ => {},
            }
        }

        resp.add_header("Connection".to_string(),
                        if conn.keep_alive { "keep-alive".to_string() }
                        else { "close".to_string() });
        if let Some(content_type) = mime {
            // If we want to add a content type, add it
            resp.add_header("Content-Type".to_string(), content_type.to_string());
        }

        // Write headers
        resp.write_headers_to_stream(&conn.stream)?;

        conn.response = Some(resp);

        conn.response_data = match req.method.unwrap() {
            HttpMethod::GET => {
                conn.bytes_requested += range;
                response_data 
            }
            HttpMethod::HEAD => ResponseDataType::None
        };

        // Force an initial write of the data
        self.write_partial_response(&mut conn)
    }

    fn write_partial_response(&self, conn: &mut HttpConnection) -> Result<ConnectionState, io::Error> {
        let done = match &mut conn.response {
            Some(ref mut resp) => {
                let amt_written = match &mut conn.response_data {
                    ResponseDataType::StringSegment(ref mut seg) => {
                        resp.partial_write_to_stream(seg, &conn.stream)?
                    }
                    ResponseDataType::FileSegment(ref mut file) => {
                        resp.partial_write_to_stream(file, &conn.stream)?
                    }
                    ResponseDataType::None => 0
                };
                conn.bytes_sent += amt_written;
                // If we wrote nothing, we are done
                amt_written == 0
            }
            None => true,
        };

        if done {
            if conn.keep_alive {
                // Reset the data associated with this connection
                conn.reset();
                return Ok(ConnectionState::ReadingRequest);
            } else {
                return Ok(ConnectionState::Closing);
            }
        }

        return Ok(ConnectionState::WritingResponse);
    }

    fn create_http_connection(stream: TcpStream) -> HttpConnection {
        HttpConnection::new(stream)
    }

    fn handle_conn_sigpipe(&self, conn: &mut HttpConnection) -> Result<(), io::Error> {
        match self.handle_conn(conn) {
            Err(error) => {
                conn.state = ConnectionState::Closing;
                match error.kind() {
                    io::ErrorKind::BrokenPipe => Ok(()),
                    // Forward the error if it wasn't broken pipe
                    _ => Err(error)
                }
            },
            _ => Ok(())
        }
    }

    fn handle_conn(&self, conn: &mut HttpConnection) -> Result<(), io::Error> {
        match conn.state {
            ConnectionState::ReadingRequest => {
                conn.state = self.read_partial_request(conn)?;
            }
            ConnectionState::WritingResponse => {
                conn.state = self.write_partial_response(conn)?;
            }
            ConnectionState::Closing => {}
        }

        Ok(())
    }

    fn write_error_response(&self, status: HttpStatus, mut conn: &mut HttpConnection, msg: Option<&str>) -> Result<ConnectionState, io::Error> {
        let body: String = rendering::render_error(&status, msg);
        let mut resp = HttpResponse::new(status, &HttpVersion::Http1_0);
        resp.add_header("Server".to_string(), "http-tui".to_string());
        resp.set_content_length(body.len());
        resp.add_header("Connection".to_string(),
                        if conn.keep_alive { "keep-alive".to_string() }
                        else { "close".to_string() });
        resp.add_header("Content-Type".to_string(), "text/html".to_string());

        // Add content-length to bytes requested
        conn.bytes_requested += body.len();

        let data = ResponseDataType::StringSegment(SeekableString::new(body));

        // Write headers
        resp.write_headers_to_stream(&conn.stream)?;

        conn.response = Some(resp);
        conn.response_data = data;

        // Force an initial write of the data
        self.write_partial_response(&mut conn)
    }
}
