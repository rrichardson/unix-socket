//! Support for Unix domain socket clients and servers.
#![warn(missing_docs)]
#![doc(html_root_url="https://doc.rust-lang.org/unix-socket/doc/v0.5.0")]

extern crate libc;

use std::ascii;
use std::cmp::Ordering;
use std::convert::AsRef;
use std::ffi::OsStr;
use std::fmt;
use std::io;
use std::iter::IntoIterator;
use std::mem;
use std::net::Shutdown;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::io::{RawFd, AsRawFd, FromRawFd, IntoRawFd};
use std::path::Path;
use std::time::Duration;

fn sun_path_offset() -> usize {
    unsafe {
        // Work with an actual instance of the type since using a null pointer is UB
        let addr: libc::sockaddr_un = mem::uninitialized();
        let base = &addr as *const _ as usize;
        let path = &addr.sun_path as *const _ as usize;
        path - base
    }
}

fn cvt(v: libc::c_int) -> io::Result<libc::c_int> {
    if v < 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(v)
    }
}

fn cvt_s(v: libc::ssize_t) -> io::Result<libc::ssize_t> {
    if v < 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(v)
    }
}

struct Inner(RawFd);

impl Drop for Inner {
    fn drop(&mut self) {
        unsafe {
            libc::close(self.0);
        }
    }
}

impl Inner {
    fn new(kind: libc::c_int) -> io::Result<Inner> {
        unsafe { cvt(libc::socket(libc::AF_UNIX, kind, 0)).map(Inner) }
    }

    fn new_pair(kind: libc::c_int) -> io::Result<(Inner, Inner)> {
        unsafe {
            let mut fds = [0, 0];
            try!(cvt(libc::socketpair(libc::AF_UNIX, kind, 0, fds.as_mut_ptr())));
            Ok((Inner(fds[0]), Inner(fds[1])))
        }
    }

    fn try_clone(&self) -> io::Result<Inner> {
        unsafe { cvt(libc::dup(self.0)).map(Inner) }
    }

    fn shutdown(&self, how: Shutdown) -> io::Result<()> {
        let how = match how {
            Shutdown::Read => libc::SHUT_RD,
            Shutdown::Write => libc::SHUT_WR,
            Shutdown::Both => libc::SHUT_RDWR,
        };

        unsafe { cvt(libc::shutdown(self.0, how)).map(|_| ()) }
    }

    fn timeout(&self, kind: libc::c_int) -> io::Result<Option<Duration>> {
        let timeout = unsafe {
            let mut timeout: libc::timeval = mem::zeroed();
            let mut size = mem::size_of::<libc::timeval>() as libc::socklen_t;
            try!(cvt(libc::getsockopt(self.0,
                                      libc::SOL_SOCKET,
                                      kind,
                                      &mut timeout as *mut _ as *mut _,
                                      &mut size as *mut _ as *mut _)));
            timeout
        };

        if timeout.tv_sec == 0 && timeout.tv_usec == 0 {
            Ok(None)
        } else {
            Ok(Some(Duration::new(timeout.tv_sec as u64, (timeout.tv_usec as u32) * 1000)))
        }
    }

    fn set_timeout(&self, dur: Option<Duration>, kind: libc::c_int) -> io::Result<()> {
        let timeout = match dur {
            Some(dur) => {
                if dur.as_secs() == 0 && dur.subsec_nanos() == 0 {
                    return Err(io::Error::new(io::ErrorKind::InvalidInput,
                                              "cannot set a 0 duration timeout"));
                }

                let (secs, usecs) = if dur.as_secs() > libc::time_t::max_value() as u64 {
                    (libc::time_t::max_value(), 999_999)
                } else {
                    (dur.as_secs() as libc::time_t,
                     (dur.subsec_nanos() / 1000) as libc::suseconds_t)
                };
                let mut timeout = libc::timeval {
                    tv_sec: secs,
                    tv_usec: usecs,
                };
                if timeout.tv_sec == 0 && timeout.tv_usec == 0 {
                    timeout.tv_usec = 1;
                }
                timeout
            }
            None => {
                libc::timeval {
                    tv_sec: 0,
                    tv_usec: 0,
                }
            }
        };

        unsafe {
            cvt(libc::setsockopt(self.0,
                                 libc::SOL_SOCKET,
                                 kind,
                                 &timeout as *const _ as *const _,
                                 mem::size_of::<libc::timeval>() as libc::socklen_t))
                .map(|_| ())
        }
    }

    fn set_nonblocking(&self, nonblocking: bool) -> io::Result<()> {
        let mut nonblocking = nonblocking as libc::c_ulong;
        unsafe { cvt(libc::ioctl(self.0, libc::FIONBIO, &mut nonblocking)).map(|_| ()) }
    }

    fn take_error(&self) -> io::Result<Option<io::Error>> {
        let mut errno: libc::c_int = 0;

        unsafe {
            try!(cvt(libc::getsockopt(self.0,
                                      libc::SOL_SOCKET,
                                      libc::SO_ERROR,
                                      &mut errno as *mut _ as *mut _,
                                      &mut mem::size_of_val(&errno) as *mut _ as *mut _)));
        }

        if errno == 0 {
            Ok(None)
        } else {
            Ok(Some(io::Error::from_raw_os_error(errno)))
        }
    }

    pub fn recv(&self, buf: &mut [u8]) -> io::Result<usize> {
        unsafe {
            let count = try!(cvt_s(libc::recv(self.0,
                                              buf.as_mut_ptr() as *mut _,
                                              buf.len(),
                                              0)));
            Ok(count as usize)
        }
    }

    pub fn send(&self, buf: &[u8]) -> io::Result<usize> {
        unsafe {
            let count = try!(cvt_s(libc::send(self.0,
                                              buf.as_ptr() as *const _,
                                              buf.len(),
                                              0)));
            Ok(count as usize)
        }
    }
}

unsafe fn sockaddr_un<P: AsRef<Path>>(path: P) -> io::Result<(libc::sockaddr_un, libc::socklen_t)> {
    let mut addr: libc::sockaddr_un = mem::zeroed();
    addr.sun_family = libc::AF_UNIX as libc::sa_family_t;

    let bytes = path.as_ref().as_os_str().as_bytes();

    match (bytes.get(0), bytes.len().cmp(&addr.sun_path.len())) {
        // Abstract paths don't need a null terminator
        (Some(&0), Ordering::Greater) => {
            return Err(io::Error::new(io::ErrorKind::InvalidInput,
                                      "path must be no longer than SUN_LEN"));
        }
        (_, Ordering::Greater) | (_, Ordering::Equal) => {
            return Err(io::Error::new(io::ErrorKind::InvalidInput,
                                      "path must be shorter than SUN_LEN"));
        }
        _ => {}
    }
    for (dst, src) in addr.sun_path.iter_mut().zip(bytes.iter()) {
        *dst = *src as libc::c_char;
    }
    // null byte for pathname addresses is already there because we zeroed the
    // struct

    let mut len = sun_path_offset() + bytes.len();
    match bytes.get(0) {
        Some(&0) | None => {}
        Some(_) => len += 1,
    }
    Ok((addr, len as libc::socklen_t))
}

enum AddressKind<'a> {
    Unnamed,
    Pathname(&'a Path),
    Abstract(&'a [u8]),
}

/// An address associated with a Unix socket.
#[derive(Clone)]
pub struct SocketAddr {
    addr: libc::sockaddr_un,
    len: libc::socklen_t,
}

impl SocketAddr {
    fn new<F>(f: F) -> io::Result<SocketAddr>
        where F: FnOnce(*mut libc::sockaddr, *mut libc::socklen_t) -> libc::c_int
    {
        unsafe {
            let mut addr: libc::sockaddr_un = mem::zeroed();
            let mut len = mem::size_of::<libc::sockaddr_un>() as libc::socklen_t;
            try!(cvt(f(&mut addr as *mut _ as *mut _, &mut len)));

            if len == 0 {
                // When there is a datagram from unnamed unix socket
                // linux returns zero bytes of address
                len = sun_path_offset() as libc::socklen_t;  // i.e. zero-length address
            } else if addr.sun_family != libc::AF_UNIX as libc::sa_family_t {
                return Err(io::Error::new(io::ErrorKind::InvalidInput,
                                          "file descriptor did not correspond to a Unix socket"));
            }

            Ok(SocketAddr {
                addr: addr,
                len: len,
            })
        }
    }

    /// Returns true iff the address is unnamed.
    pub fn is_unnamed(&self) -> bool {
        if let AddressKind::Unnamed = self.address() {
            true
        } else {
            false
        }
    }

    /// Returns the contents of this address if it is a `pathname` address.
    pub fn as_pathname(&self) -> Option<&Path> {
        if let AddressKind::Pathname(path) = self.address() {
            Some(path)
        } else {
            None
        }
    }

    fn address<'a>(&'a self) -> AddressKind<'a> {
        let len = self.len as usize - sun_path_offset();
        let path = unsafe { mem::transmute::<&[libc::c_char], &[u8]>(&self.addr.sun_path) };

        // OSX seems to return a len of 16 and a zeroed sun_path for unnamed addresses
        if len == 0 || (cfg!(not(target_os = "linux")) && self.addr.sun_path[0] == 0) {
            AddressKind::Unnamed
        } else if self.addr.sun_path[0] == 0 {
            AddressKind::Abstract(&path[1..len])
        } else {
            AddressKind::Pathname(OsStr::from_bytes(&path[..len - 1]).as_ref())
        }
    }
}

impl fmt::Debug for SocketAddr {
    fn fmt(&self, fmt: &mut fmt::Formatter) -> fmt::Result {
        match self.address() {
            AddressKind::Unnamed => write!(fmt, "(unnamed)"),
            AddressKind::Abstract(name) => write!(fmt, "{} (abstract)", AsciiEscaped(name)),
            AddressKind::Pathname(path) => write!(fmt, "{:?} (pathname)", path),
        }
    }
}

struct AsciiEscaped<'a>(&'a [u8]);

impl<'a> fmt::Display for AsciiEscaped<'a> {
    fn fmt(&self, fmt: &mut fmt::Formatter) -> fmt::Result {
        try!(write!(fmt, "\""));
        for byte in self.0.iter().cloned().flat_map(ascii::escape_default) {
            try!(write!(fmt, "{}", byte as char));
        }
        write!(fmt, "\"")
    }
}

/// OS specific extension traits.
pub mod os {
    /// Linux specific extension traits.
    #[cfg(target_os = "linux")]
    pub mod linux {
        use {AddressKind, SocketAddr};

        /// Linux specific extensions for the `SocketAddr` type.
        pub trait SocketAddrExt {
            /// Returns the contents of this address (without the leading
            /// null byte) if it is an `abstract` address.
            fn as_abstract(&self) -> Option<&[u8]>;
        }

        impl SocketAddrExt for SocketAddr {
            fn as_abstract(&self) -> Option<&[u8]> {
                if let AddressKind::Abstract(path) = self.address() {
                    Some(path)
                } else {
                    None
                }
            }
        }
    }
}

/// A Unix stream socket.
///
/// # Examples
///
/// ```rust,no_run
/// use unix_socket::UnixStream;
/// use std::io::prelude::*;
///
/// let mut stream = UnixStream::connect("/path/to/my/socket").unwrap();
/// stream.write_all(b"hello world").unwrap();
/// let mut response = String::new();
/// stream.read_to_string(&mut response).unwrap();
/// println!("{}", response);
/// ```
pub struct UnixStream {
    inner: Inner,
}

impl fmt::Debug for UnixStream {
    fn fmt(&self, fmt: &mut fmt::Formatter) -> fmt::Result {
        let mut builder = fmt.debug_struct("UnixStream");
        builder.field("fd", &self.inner.0);
        if let Ok(addr) = self.local_addr() {
            builder.field("local", &addr);
        }
        if let Ok(addr) = self.peer_addr() {
            builder.field("peer", &addr);
        }
        builder.finish()
    }
}

impl UnixStream {
    /// Connects to the socket named by `path`.
    ///
    /// Linux provides, as a nonportable extension, a separate "abstract"
    /// address namespace as opposed to filesystem-based addressing. If `path`
    /// begins with a null byte, it will be interpreted as an "abstract"
    /// address. Otherwise, it will be interpreted as a "pathname" address,
    /// corresponding to a path on the filesystem.
    pub fn connect<P: AsRef<Path>>(path: P) -> io::Result<UnixStream> {
        unsafe {
            let inner = try!(Inner::new(libc::SOCK_STREAM));
            let (addr, len) = try!(sockaddr_un(path));

            let ret = libc::connect(inner.0, &addr as *const _ as *const _, len);
            if ret < 0 {
                Err(io::Error::last_os_error())
            } else {
                Ok(UnixStream { inner: inner })
            }
        }
    }

    /// Creates an unnamed pair of connected sockets.
    ///
    /// Returns two `UnixStream`s which are connected to each other.
    pub fn pair() -> io::Result<(UnixStream, UnixStream)> {
        let (i1, i2) = try!(Inner::new_pair(libc::SOCK_STREAM));
        Ok((UnixStream { inner: i1 }, UnixStream { inner: i2 }))
    }

    /// Creates a new independently owned handle to the underlying socket.
    ///
    /// The returned `UnixStream` is a reference to the same stream that this
    /// object references. Both handles will read and write the same stream of
    /// data, and options set on one stream will be propogated to the other
    /// stream.
    pub fn try_clone(&self) -> io::Result<UnixStream> {
        Ok(UnixStream { inner: try!(self.inner.try_clone()) })
    }

    /// Returns the socket address of the local half of this connection.
    pub fn local_addr(&self) -> io::Result<SocketAddr> {
        SocketAddr::new(|addr, len| unsafe { libc::getsockname(self.inner.0, addr, len) })
    }

    /// Returns the socket address of the remote half of this connection.
    pub fn peer_addr(&self) -> io::Result<SocketAddr> {
        SocketAddr::new(|addr, len| unsafe { libc::getpeername(self.inner.0, addr, len) })
    }

    /// Sets the read timeout for the socket.
    ///
    /// If the provided value is `None`, then `read` calls will block
    /// indefinitely. It is an error to pass the zero `Duration` to this
    /// method.
    pub fn set_read_timeout(&self, timeout: Option<Duration>) -> io::Result<()> {
        self.inner.set_timeout(timeout, libc::SO_RCVTIMEO)
    }

    /// Sets the write timeout for the socket.
    ///
    /// If the provided value is `None`, then `write` calls will block
    /// indefinitely. It is an error to pass the zero `Duration` to this
    /// method.
    pub fn set_write_timeout(&self, timeout: Option<Duration>) -> io::Result<()> {
        self.inner.set_timeout(timeout, libc::SO_SNDTIMEO)
    }

    /// Returns the read timeout of this socket.
    pub fn read_timeout(&self) -> io::Result<Option<Duration>> {
        self.inner.timeout(libc::SO_RCVTIMEO)
    }

    /// Returns the write timeout of this socket.
    pub fn write_timeout(&self) -> io::Result<Option<Duration>> {
        self.inner.timeout(libc::SO_SNDTIMEO)
    }

    /// Moves the socket into or out of nonblocking mode.
    pub fn set_nonblocking(&self, nonblocking: bool) -> io::Result<()> {
        self.inner.set_nonblocking(nonblocking)
    }

    /// Returns the value of the `SO_ERROR` option.
    pub fn take_error(&self) -> io::Result<Option<io::Error>> {
        self.inner.take_error()
    }

    /// Shuts down the read, write, or both halves of this connection.
    ///
    /// This function will cause all pending and future I/O calls on the
    /// specified portions to immediately return with an appropriate value
    /// (see the documentation of `Shutdown`).
    pub fn shutdown(&self, how: Shutdown) -> io::Result<()> {
        self.inner.shutdown(how)
    }
}

impl io::Read for UnixStream {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        io::Read::read(&mut &*self, buf)
    }
}

impl<'a> io::Read for &'a UnixStream {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        self.inner.recv(buf)
    }
}

impl io::Write for UnixStream {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        io::Write::write(&mut &*self, buf)
    }

    fn flush(&mut self) -> io::Result<()> {
        io::Write::flush(&mut &*self)
    }
}

impl<'a> io::Write for &'a UnixStream {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.inner.send(buf)
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

impl AsRawFd for UnixStream {
    fn as_raw_fd(&self) -> RawFd {
        self.inner.0
    }
}

impl FromRawFd for UnixStream {
    unsafe fn from_raw_fd(fd: RawFd) -> UnixStream {
        UnixStream { inner: Inner(fd) }
    }
}

impl IntoRawFd for UnixStream {
    fn into_raw_fd(self) -> RawFd {
        let fd = self.inner.0;
        mem::forget(self);
        fd
    }
}


/// A structure representing a Unix domain seqpacket socket server.
///
/// # Examples
///
/// ```rust,no_run
/// use std::thread;
/// use unix_socket::{UnixSeqpacket, UnixSeqpacketListener};
///
/// fn handle_client(_stream: UnixSeqpacket) {
///     // ...
/// }
///
/// let listener = UnixSeqpacketListener::bind("/path/to/the/socket").unwrap();
///
/// // accept connections and process them, spawning a new thread for each one
/// for sock in listener.incoming() {
///     match sock {
///         Ok(sock) => {
///             /* connection succeeded */
///             thread::spawn(|| handle_client(sock));
///         }
///         Err(_err) => {
///             /* connection failed */
///             break;
///         }
///     }
/// }
///
/// // close the listener socket
/// drop(listener);
/// ```
pub struct UnixSeqpacketListener {
    inner: Inner,
}

impl fmt::Debug for UnixSeqpacketListener {
    fn fmt(&self, fmt: &mut fmt::Formatter) -> fmt::Result {
        let mut builder = fmt.debug_struct("UnixSeqpacketListener");
        builder.field("fd", &self.inner.0);
        if let Ok(addr) = self.local_addr() {
            builder.field("local", &addr);
        }
        builder.finish()
    }
}

impl UnixSeqpacketListener {
    /// Creates a new `UnixSeqpacketListener` bound to the specified socket.
    ///
    /// Linux provides, as a nonportable extension, a separate "abstract"
    /// address namespace as opposed to filesystem-based addressing. If `path`
    /// begins with a null byte, it will be interpreted as an "abstract"
    /// address. Otherwise, it will be interpreted as a "pathname" address,
    /// corresponding to a path on the filesystem.
    pub fn bind<P: AsRef<Path>>(path: P) -> io::Result<UnixSeqpacketListener> {
        unsafe {
            let inner = try!(Inner::new(libc::SOCK_SEQPACKET));
            let (addr, len) = try!(sockaddr_un(path));

            try!(cvt(libc::bind(inner.0, &addr as *const _ as *const _, len)));
            try!(cvt(libc::listen(inner.0, 128)));

            Ok(UnixSeqpacketListener { inner: inner })
        }
    }

    /// Accepts a new incoming connection to this listener.
    ///
    /// This function will block the calling thread until a new Unix connection
    /// is established. When established, the corersponding `UnixSeqpacket` and
    /// the remote peer's address will be returned.
    pub fn accept(&self) -> io::Result<(UnixSeqpacket, SocketAddr)> {
        unsafe {
            let mut fd = 0;
            let addr = try!(SocketAddr::new(|addr, len| {
                fd = libc::accept(self.inner.0, addr, len);
                fd
            }));

            Ok((UnixSeqpacket { inner: Inner(fd) }, addr))
        }
    }

    /// Creates a new independently owned handle to the underlying socket.
    ///
    /// The returned `UnixSeqpacketListener` is a reference to the same socket that this
    /// object references. Both handles can be used to accept incoming
    /// connections and options set on one listener will affect the other.
    pub fn try_clone(&self) -> io::Result<UnixSeqpacketListener> {
        Ok(UnixSeqpacketListener { inner: try!(self.inner.try_clone()) })
    }

    /// Returns the local socket address of this listener.
    pub fn local_addr(&self) -> io::Result<SocketAddr> {
        SocketAddr::new(|addr, len| unsafe { libc::getsockname(self.inner.0, addr, len) })
    }

    /// Moves the socket into or out of nonblocking mode.
    pub fn set_nonblocking(&self, nonblocking: bool) -> io::Result<()> {
        self.inner.set_nonblocking(nonblocking)
    }

    /// Returns the value of the `SO_ERROR` option.
    pub fn take_error(&self) -> io::Result<Option<io::Error>> {
        self.inner.take_error()
    }

    /// Returns an iterator over incoming connections.
    ///
    /// The iterator will never return `None` and will also not yield the
    /// peer's `SocketAddr` structure.
    pub fn incoming<'a>(&'a self) -> IncomingSeqpacket<'a> {
        IncomingSeqpacket { listener: self }
    }
}

impl AsRawFd for UnixSeqpacketListener {
    fn as_raw_fd(&self) -> RawFd {
        self.inner.0
    }
}

impl FromRawFd for UnixSeqpacketListener {
    unsafe fn from_raw_fd(fd: RawFd) -> UnixSeqpacketListener {
        UnixSeqpacketListener { inner: Inner(fd) }
    }
}

impl IntoRawFd for UnixSeqpacketListener {
    fn into_raw_fd(self) -> RawFd {
        let fd = self.inner.0;
        mem::forget(self);
        fd
    }
}

impl<'a> IntoIterator for &'a UnixSeqpacketListener {
    type Item = io::Result<UnixSeqpacket>;
    type IntoIter = IncomingSeqpacket<'a>;

    fn into_iter(self) -> IncomingSeqpacket<'a> {
        self.incoming()
    }
}

/// An iterator over incoming connections to a `UnixSeqpacketListener`.
///
/// It will never return `None`.
#[derive(Debug)]
pub struct IncomingSeqpacket<'a> {
    listener: &'a UnixSeqpacketListener,
}

impl<'a> Iterator for IncomingSeqpacket<'a> {
    type Item = io::Result<UnixSeqpacket>;

    fn next(&mut self) -> Option<io::Result<UnixSeqpacket>> {
        Some(self.listener.accept().map(|s| s.0))
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        (usize::max_value(), None)
    }
}



/// A structure representing a Unix domain stream socket server.
///
/// # Examples
///
/// ```rust,no_run
/// use std::thread;
/// use unix_socket::{UnixStream, UnixStreamListener};
///
/// fn handle_client(_stream: UnixStream) {
///     // ...
/// }
///
/// let listener = UnixStreamListener::bind("/path/to/the/socket").unwrap();
///
/// // accept connections and process them, spawning a new thread for each one
/// for stream in listener.incoming() {
///     match stream {
///         Ok(stream) => {
///             /* connection succeeded */
///             thread::spawn(|| handle_client(stream));
///         }
///         Err(_err) => {
///             /* connection failed */
///             break;
///         }
///     }
/// }
///
/// // close the listener socket
/// drop(listener);
/// ```
pub struct UnixStreamListener {
    inner: Inner,
}

impl fmt::Debug for UnixStreamListener {
    fn fmt(&self, fmt: &mut fmt::Formatter) -> fmt::Result {
        let mut builder = fmt.debug_struct("UnixStreamListener");
        builder.field("fd", &self.inner.0);
        if let Ok(addr) = self.local_addr() {
            builder.field("local", &addr);
        }
        builder.finish()
    }
}

impl UnixStreamListener {
    /// Creates a new `UnixStreamListener` bound to the specified socket.
    ///
    /// Linux provides, as a nonportable extension, a separate "abstract"
    /// address namespace as opposed to filesystem-based addressing. If `path`
    /// begins with a null byte, it will be interpreted as an "abstract"
    /// address. Otherwise, it will be interpreted as a "pathname" address,
    /// corresponding to a path on the filesystem.
    pub fn bind<P: AsRef<Path>>(path: P) -> io::Result<UnixStreamListener> {
        unsafe {
            let inner = try!(Inner::new(libc::SOCK_STREAM));
            let (addr, len) = try!(sockaddr_un(path));

            try!(cvt(libc::bind(inner.0, &addr as *const _ as *const _, len)));
            try!(cvt(libc::listen(inner.0, 128)));

            Ok(UnixStreamListener { inner: inner })
        }
    }

    /// Accepts a new incoming connection to this listener.
    ///
    /// This function will block the calling thread until a new Unix connection
    /// is established. When established, the corersponding `UnixStream` and
    /// the remote peer's address will be returned.
    pub fn accept(&self) -> io::Result<(UnixStream, SocketAddr)> {
        unsafe {
            let mut fd = 0;
            let addr = try!(SocketAddr::new(|addr, len| {
                fd = libc::accept(self.inner.0, addr, len);
                fd
            }));

            Ok((UnixStream { inner: Inner(fd) }, addr))
        }
    }

    /// Creates a new independently owned handle to the underlying socket.
    ///
    /// The returned `UnixStreamListener` is a reference to the same socket that this
    /// object references. Both handles can be used to accept incoming
    /// connections and options set on one listener will affect the other.
    pub fn try_clone(&self) -> io::Result<UnixStreamListener> {
        Ok(UnixStreamListener { inner: try!(self.inner.try_clone()) })
    }

    /// Returns the local socket address of this listener.
    pub fn local_addr(&self) -> io::Result<SocketAddr> {
        SocketAddr::new(|addr, len| unsafe { libc::getsockname(self.inner.0, addr, len) })
    }

    /// Moves the socket into or out of nonblocking mode.
    pub fn set_nonblocking(&self, nonblocking: bool) -> io::Result<()> {
        self.inner.set_nonblocking(nonblocking)
    }

    /// Returns the value of the `SO_ERROR` option.
    pub fn take_error(&self) -> io::Result<Option<io::Error>> {
        self.inner.take_error()
    }

    /// Returns an iterator over incoming connections.
    ///
    /// The iterator will never return `None` and will also not yield the
    /// peer's `SocketAddr` structure.
    pub fn incoming<'a>(&'a self) -> IncomingStream<'a> {
        IncomingStream { listener: self }
    }
}

impl AsRawFd for UnixStreamListener {
    fn as_raw_fd(&self) -> RawFd {
        self.inner.0
    }
}

impl FromRawFd for UnixStreamListener {
    unsafe fn from_raw_fd(fd: RawFd) -> UnixStreamListener {
        UnixStreamListener { inner: Inner(fd) }
    }
}

impl IntoRawFd for UnixStreamListener {
    fn into_raw_fd(self) -> RawFd {
        let fd = self.inner.0;
        mem::forget(self);
        fd
    }
}

impl<'a> IntoIterator for &'a UnixStreamListener {
    type Item = io::Result<UnixStream>;
    type IntoIter = IncomingStream<'a>;

    fn into_iter(self) -> IncomingStream<'a> {
        self.incoming()
    }
}

/// An iterator over incoming connections to a `UnixStreamListener`.
///
/// It will never return `None`.
#[derive(Debug)]
pub struct IncomingStream<'a> {
    listener: &'a UnixStreamListener,
}

impl<'a> Iterator for IncomingStream<'a> {
    type Item = io::Result<UnixStream>;

    fn next(&mut self) -> Option<io::Result<UnixStream>> {
        Some(self.listener.accept().map(|s| s.0))
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        (usize::max_value(), None)
    }
}

/// A Unix datagram socket.
///
/// # Examples
///
/// ```rust,no_run
/// use unix_socket::UnixDatagram;
///
/// let socket = UnixDatagram::bind("/path/to/my/socket").unwrap();
/// socket.send_to(b"hello world", "/path/to/other/socket").unwrap();
/// let mut buf = [0; 100];
/// let (count, address) = socket.recv_from(&mut buf).unwrap();
/// println!("socket {:?} sent {:?}", address, &buf[..count]);
/// ```
pub struct UnixDatagram {
    inner: Inner,
}

impl fmt::Debug for UnixDatagram {
    fn fmt(&self, fmt: &mut fmt::Formatter) -> fmt::Result {
        let mut builder = fmt.debug_struct("UnixDatagram");
        builder.field("fd", &self.inner.0);
        if let Ok(addr) = self.local_addr() {
            builder.field("local", &addr);
        }
        if let Ok(addr) = self.peer_addr() {
            builder.field("peer", &addr);
        }
        builder.finish()
    }
}

impl UnixDatagram {
    /// Creates a Unix datagram socket bound to the given path.
    ///
    /// Linux provides, as a nonportable extension, a separate "abstract"
    /// address namespace as opposed to filesystem-based addressing. If `path`
    /// begins with a null byte, it will be interpreted as an "abstract"
    /// address. Otherwise, it will be interpreted as a "pathname" address,
    /// corresponding to a path on the filesystem.
    pub fn bind<P: AsRef<Path>>(path: P) -> io::Result<UnixDatagram> {
        unsafe {
            let inner = try!(Inner::new(libc::SOCK_DGRAM));
            let (addr, len) = try!(sockaddr_un(path));

            try!(cvt(libc::bind(inner.0, &addr as *const _ as *const _, len)));

            Ok(UnixDatagram { inner: inner })
        }
    }

    /// Creates a Unix Datagram socket which is not bound to any address.
    pub fn unbound() -> io::Result<UnixDatagram> {
        let inner = try!(Inner::new(libc::SOCK_DGRAM));
        Ok(UnixDatagram { inner: inner })
    }

    /// Create an unnamed pair of connected sockets.
    ///
    /// Returns two `UnixDatagrams`s which are connected to each other.
    pub fn pair() -> io::Result<(UnixDatagram, UnixDatagram)> {
        let (i1, i2) = try!(Inner::new_pair(libc::SOCK_DGRAM));
        Ok((UnixDatagram { inner: i1 }, UnixDatagram { inner: i2 }))
    }

    /// Connects the socket to the specified address.
    ///
    /// The `send` method may be used to send data to the specified address.
    /// `recv` and `recv_from` will only receive data from that address.
    pub fn connect<P: AsRef<Path>>(&self, path: P) -> io::Result<()> {
        unsafe {
            let (addr, len) = try!(sockaddr_un(path));

            try!(cvt(libc::connect(self.inner.0, &addr as *const _ as *const _, len)));

            Ok(())
        }
    }

    /// Creates a new independently owned handle to the underlying socket.
    ///
    /// The returned `UnixStreamListener` is a reference to the same socket that this
    /// object references. Both handles can be used to accept incoming
    /// connections and options set on one listener will affect the other.
    pub fn try_clone(&self) -> io::Result<UnixDatagram> {
        Ok(UnixDatagram { inner: try!(self.inner.try_clone()) })
    }

    /// Returns the address of this socket.
    pub fn local_addr(&self) -> io::Result<SocketAddr> {
        SocketAddr::new(|addr, len| unsafe { libc::getsockname(self.inner.0, addr, len) })
    }

    /// Returns the address of this socket's peer.
    ///
    /// The `connect` method will connect the socket to a peer.
    pub fn peer_addr(&self) -> io::Result<SocketAddr> {
        SocketAddr::new(|addr, len| unsafe { libc::getpeername(self.inner.0, addr, len) })
    }

    /// Receives data from the socket.
    ///
    /// On success, returns the number of bytes read and the address from
    /// whence the data came.
    pub fn recv_from(&self, buf: &mut [u8]) -> io::Result<(usize, SocketAddr)> {
        let mut count = 0;
        let addr = try!(SocketAddr::new(|addr, len| {
            unsafe {
                count = libc::recvfrom(self.inner.0,
                                       buf.as_mut_ptr() as *mut _,
                                       buf.len(),
                                       0,
                                       addr,
                                       len);
                if count > 0 {
                    1
                } else if count == 0 {
                    0
                } else {
                    -1
                }
            }
        }));

        Ok((count as usize, addr))
    }

    /// Receives data from the socket.
    ///
    /// On success, returns the number of bytes read.
    pub fn recv(&self, buf: &mut [u8]) -> io::Result<usize> {
        self.inner.recv(buf)
    }

    /// Sends data on the socket to the specified address.
    ///
    /// On success, returns the number of bytes written.
    pub fn send_to<P: AsRef<Path>>(&self, buf: &[u8], path: P) -> io::Result<usize> {
        unsafe {
            let (addr, len) = try!(sockaddr_un(path));

            let count = try!(cvt_s(libc::sendto(self.inner.0,
                                                buf.as_ptr() as *const _,
                                                buf.len(),
                                                0,
                                                &addr as *const _ as *const _,
                                                len)));
            Ok(count as usize)
        }
    }

    /// Sends data on the socket to the socket's peer.
    ///
    /// The peer address may be set by the `connect` method, and this method
    /// will return an error if the socket has not already been connected.
    ///
    /// On success, returns the number of bytes written.
    pub fn send(&self, buf: &[u8]) -> io::Result<usize> {
        self.inner.send(buf)
    }

    /// Sets the read timeout for the socket.
    ///
    /// If the provided value is `None`, then `recv` and `recv_from` calls will
    /// block indefinitely. It is an error to pass the zero `Duration` to this
    /// method.
    pub fn set_read_timeout(&self, timeout: Option<Duration>) -> io::Result<()> {
        self.inner.set_timeout(timeout, libc::SO_RCVTIMEO)
    }

    /// Sets the write timeout for the socket.
    ///
    /// If the provided value is `None`, then `send` and `send_to` calls will
    /// block indefinitely. It is an error to pass the zero `Duration` to this
    /// method.
    pub fn set_write_timeout(&self, timeout: Option<Duration>) -> io::Result<()> {
        self.inner.set_timeout(timeout, libc::SO_SNDTIMEO)
    }

    /// Returns the read timeout of this socket.
    pub fn read_timeout(&self) -> io::Result<Option<Duration>> {
        self.inner.timeout(libc::SO_RCVTIMEO)
    }

    /// Returns the write timeout of this socket.
    pub fn write_timeout(&self) -> io::Result<Option<Duration>> {
        self.inner.timeout(libc::SO_SNDTIMEO)
    }

    /// Moves the socket into or out of nonblocking mode.
    pub fn set_nonblocking(&self, nonblocking: bool) -> io::Result<()> {
        self.inner.set_nonblocking(nonblocking)
    }

    /// Returns the value of the `SO_ERROR` option.
    pub fn take_error(&self) -> io::Result<Option<io::Error>> {
        self.inner.take_error()
    }

    /// Shut down the read, write, or both halves of this connection.
    ///
    /// This function will cause all pending and future I/O calls on the
    /// specified portions to immediately return with an appropriate value
    /// (see the documentation of `Shutdown`).
    pub fn shutdown(&self, how: Shutdown) -> io::Result<()> {
        self.inner.shutdown(how)
    }
}

impl AsRawFd for UnixDatagram {
    fn as_raw_fd(&self) -> RawFd {
        self.inner.0
    }
}

impl FromRawFd for UnixDatagram {
    unsafe fn from_raw_fd(fd: RawFd) -> UnixDatagram {
        UnixDatagram { inner: Inner(fd) }
    }
}

impl IntoRawFd for UnixDatagram {
    fn into_raw_fd(self) -> RawFd {
        let fd = self.inner.0;
        mem::forget(self);
        fd
    }
}

/// A Unix seqpacket socket.
///
/// A Unix Seqpacket socket is connection oriented but sends and receives
/// datagrams with guaranteed ordering.
///
/// # Examples
///
/// ```rust,no_run
/// use unix_socket::UnixSeqpacket;
///
/// let path = "/path/to/my/socket";
/// let socket = UnixSeqpacket::connect(path).unwrap();
/// let _count = socket.send(b"hello world").unwrap();
/// let mut buf = [0; 100];
/// let count = socket.recv(&mut buf).unwrap();
/// println!("socket {:?} sent {:?}", path, &buf[..count]);
/// ```
pub struct UnixSeqpacket {
    inner: Inner,
}

impl fmt::Debug for UnixSeqpacket {
    fn fmt(&self, fmt: &mut fmt::Formatter) -> fmt::Result {
        let mut builder = fmt.debug_struct("UnixSeqpacket");
        builder.field("fd", &self.inner.0);
        if let Ok(addr) = self.local_addr() {
            builder.field("local", &addr);
        }
        if let Ok(addr) = self.peer_addr() {
            builder.field("peer", &addr);
        }
        builder.finish()
    }
}

impl UnixSeqpacket {
    /// Connects to the socket named by `path`.
    ///
    /// Linux provides, as a nonportable extension, a separate "abstract"
    /// address namespace as opposed to filesystem-based addressing. If `path`
    /// begins with a null byte, it will be interpreted as an "abstract"
    /// address. Otherwise, it will be interpreted as a "pathname" address,
    /// corresponding to a path on the filesystem.
    pub fn connect<P: AsRef<Path>>(path: P) -> io::Result<UnixSeqpacket> {
        unsafe {
            let inner = try!(Inner::new(libc::SOCK_SEQPACKET));
            let (addr, len) = try!(sockaddr_un(path));

            let ret = libc::connect(inner.0, &addr as *const _ as *const _, len);
            if ret < 0 {
                Err(io::Error::last_os_error())
            } else {
                Ok(UnixSeqpacket { inner: inner })
            }
        }
    }

    /// Create an unnamed pair of connected sockets.
    ///
    /// Returns two `UnixSeqpackets`s which are connected to each other.
    pub fn pair() -> io::Result<(UnixSeqpacket, UnixSeqpacket)> {
        let (i1, i2) = try!(Inner::new_pair(libc::SOCK_SEQPACKET));
        Ok((UnixSeqpacket { inner: i1 }, UnixSeqpacket { inner: i2 }))
    }

    /// Creates a new independently owned handle to the underlying socket.
    ///
    /// The returned `UnixStreamListener` is a reference to the same socket that this
    /// object references. Both handles can be used to accept incoming
    /// connections and options set on one listener will affect the other.
    pub fn try_clone(&self) -> io::Result<UnixSeqpacket> {
        Ok(UnixSeqpacket { inner: try!(self.inner.try_clone()) })
    }

    /// Returns the address of this socket.
    pub fn local_addr(&self) -> io::Result<SocketAddr> {
        SocketAddr::new(|addr, len| unsafe { libc::getsockname(self.inner.0, addr, len) })
    }

    /// Returns the address of this socket's peer.
    ///
    /// Returns the SocketAddr (path) of the peer of this connected socket
    pub fn peer_addr(&self) -> io::Result<SocketAddr> {
        SocketAddr::new(|addr, len| unsafe { libc::getpeername(self.inner.0, addr, len) })
    }

    /// Receives data from the socket from the connected peer.
    ///
    /// On success, returns the number of bytes read.
    pub fn recv(&self, buf: &mut [u8]) -> io::Result<usize> {
        self.inner.recv(buf)
    }

    /// Sends data on the socket to the socket's peer.
    ///
    /// will return an error if the socket has not already been connected.
    ///
    /// On success, returns the number of bytes written.
    pub fn send(&self, buf: &[u8]) -> io::Result<usize> {
        self.inner.send(buf)
    }

    /// Sets the read timeout for the socket.
    ///
    /// If the provided value is `None`, then `recv` and `recv_from` calls will
    /// block indefinitely. It is an error to pass the zero `Duration` to this
    /// method.
    pub fn set_read_timeout(&self, timeout: Option<Duration>) -> io::Result<()> {
        self.inner.set_timeout(timeout, libc::SO_RCVTIMEO)
    }

    /// Sets the write timeout for the socket.
    ///
    /// If the provided value is `None`, then `send` and `send_to` calls will
    /// block indefinitely. It is an error to pass the zero `Duration` to this
    /// method.
    pub fn set_write_timeout(&self, timeout: Option<Duration>) -> io::Result<()> {
        self.inner.set_timeout(timeout, libc::SO_SNDTIMEO)
    }

    /// Returns the read timeout of this socket.
    pub fn read_timeout(&self) -> io::Result<Option<Duration>> {
        self.inner.timeout(libc::SO_RCVTIMEO)
    }

    /// Returns the write timeout of this socket.
    pub fn write_timeout(&self) -> io::Result<Option<Duration>> {
        self.inner.timeout(libc::SO_SNDTIMEO)
    }

    /// Moves the socket into or out of nonblocking mode.
    pub fn set_nonblocking(&self, nonblocking: bool) -> io::Result<()> {
        self.inner.set_nonblocking(nonblocking)
    }

    /// Returns the value of the `SO_ERROR` option.
    pub fn take_error(&self) -> io::Result<Option<io::Error>> {
        self.inner.take_error()
    }

    /// Shut down the read, write, or both halves of this connection.
    ///
    /// This function will cause all pending and future I/O calls on the
    /// specified portions to immediately return with an appropriate value
    /// (see the documentation of `Shutdown`).
    pub fn shutdown(&self, how: Shutdown) -> io::Result<()> {
        self.inner.shutdown(how)
    }
}

impl AsRawFd for UnixSeqpacket {
    fn as_raw_fd(&self) -> RawFd {
        self.inner.0
    }
}

impl FromRawFd for UnixSeqpacket {
    unsafe fn from_raw_fd(fd: RawFd) -> UnixSeqpacket {
        UnixSeqpacket { inner: Inner(fd) }
    }
}

impl IntoRawFd for UnixSeqpacket {
    fn into_raw_fd(self) -> RawFd {
        let fd = self.inner.0;
        mem::forget(self);
        fd
    }
}


#[cfg(test)]
mod test {
    extern crate tempdir;

    use std::thread;
    use std::io;
    use std::io::prelude::*;
    use std::time::Duration;
    use self::tempdir::TempDir;
    use std::net::Shutdown;

    use super::*;

    macro_rules! or_panic {
        ($e:expr) => {
            match $e {
                Ok(e) => e,
                Err(e) => panic!("{}", e),
            }
        }
    }

    #[test]
    fn basic_stream() {
        let dir = or_panic!(TempDir::new("unix_socket"));
        let socket_path = dir.path().join("sock");
        let msg1 = b"hello";
        let msg2 = b"world!";

        let listener = or_panic!(UnixStreamListener::bind(&socket_path));
        let thread = thread::spawn(move || {
            let mut stream = or_panic!(listener.accept()).0;
            let mut buf = [0; 5];
            or_panic!(stream.read(&mut buf));
            assert_eq!(&msg1[..], &buf[..]);
            or_panic!(stream.write_all(msg2));
        });

        let mut stream = or_panic!(UnixStream::connect(&socket_path));
        assert_eq!(Some(&*socket_path),
                   stream.peer_addr().unwrap().as_pathname());
        or_panic!(stream.write_all(msg1));
        let mut buf = vec![];
        or_panic!(stream.read_to_end(&mut buf));
        assert_eq!(&msg2[..], &buf[..]);
        drop(stream);

        thread.join().unwrap();
    }

    #[test]
    fn basic_seqpacket() {
        let dir = or_panic!(TempDir::new("unix_socket"));
        let socket_path = dir.path().join("sock");
        let msg1 = b"hello";
        let msg2 = b"world!";

        let listener = or_panic!(UnixSeqpacketListener::bind(&socket_path));
        let thread = thread::spawn(move || {
            let stream = or_panic!(listener.accept()).0;
            let mut buf = [0; 5];
            let res = or_panic!(stream.recv(&mut buf));
            println!("recv in thread result was {}", res);
            assert_eq!(&msg1[..], &buf[..]);
            let res = or_panic!(stream.send(msg2));
            println!("send in thread result was {}", res);
        });

        let stream = or_panic!(UnixSeqpacket::connect(&socket_path));
        assert_eq!(Some(&*socket_path),
                   stream.peer_addr().unwrap().as_pathname());
        let res = or_panic!(stream.send(msg1));
        println!("outer send result was {}", res);
        let mut buf = vec![0,0,0,0,0,0];
        let res = or_panic!(stream.recv(&mut buf));
        println!("outer recv result was {}", res);
        assert_eq!(&msg2[..], &buf[..]);
        drop(stream);

        thread.join().unwrap();
    }


    #[test]
    fn pair() {
        let msg1 = b"hello";
        let msg2 = b"world!";

        let (mut s1, mut s2) = or_panic!(UnixStream::pair());
        let thread = thread::spawn(move || {
            // s1 must be moved in or the test will hang!
            let mut buf = [0; 5];
            or_panic!(s1.read(&mut buf));
            assert_eq!(&msg1[..], &buf[..]);
            or_panic!(s1.write_all(msg2));
        });

        or_panic!(s2.write_all(msg1));
        let mut buf = vec![];
        or_panic!(s2.read_to_end(&mut buf));
        assert_eq!(&msg2[..], &buf[..]);
        drop(s2);

        thread.join().unwrap();
    }

    #[test]
    #[cfg(target_os = "linux")]
    fn abstract_address() {
        use os::linux::SocketAddrExt;

        let socket_path = "\0the path";
        let msg1 = b"hello";
        let msg2 = b"world!";

        let listener = or_panic!(UnixStreamListener::bind(&socket_path));
        let thread = thread::spawn(move || {
            let mut stream = or_panic!(listener.accept()).0;
            let mut buf = [0; 5];
            or_panic!(stream.read(&mut buf));
            assert_eq!(&msg1[..], &buf[..]);
            or_panic!(stream.write_all(msg2));
        });

        let mut stream = or_panic!(UnixStream::connect(&socket_path));
        assert_eq!(Some(&b"the path"[..]),
                   stream.peer_addr().unwrap().as_abstract());
        or_panic!(stream.write_all(msg1));
        let mut buf = vec![];
        or_panic!(stream.read_to_end(&mut buf));
        assert_eq!(&msg2[..], &buf[..]);
        drop(stream);

        thread.join().unwrap();
    }

    #[test]
    fn try_clone() {
        let dir = or_panic!(TempDir::new("unix_socket"));
        let socket_path = dir.path().join("sock");
        let msg1 = b"hello";
        let msg2 = b"world";

        let listener = or_panic!(UnixStreamListener::bind(&socket_path));
        let thread = thread::spawn(move || {
            let mut stream = or_panic!(listener.accept()).0;
            or_panic!(stream.write_all(msg1));
            or_panic!(stream.write_all(msg2));
        });

        let mut stream = or_panic!(UnixStream::connect(&socket_path));
        let mut stream2 = or_panic!(stream.try_clone());

        let mut buf = [0; 5];
        or_panic!(stream.read(&mut buf));
        assert_eq!(&msg1[..], &buf[..]);
        or_panic!(stream2.read(&mut buf));
        assert_eq!(&msg2[..], &buf[..]);

        thread.join().unwrap();
    }

    #[test]
    fn iter() {
        let dir = or_panic!(TempDir::new("unix_socket"));
        let socket_path = dir.path().join("sock");

        let listener = or_panic!(UnixStreamListener::bind(&socket_path));
        let thread = thread::spawn(move || {
            for stream in listener.incoming().take(2) {
                let mut stream = or_panic!(stream);
                let mut buf = [0];
                or_panic!(stream.read(&mut buf));
            }
        });

        for _ in 0..2 {
            let mut stream = or_panic!(UnixStream::connect(&socket_path));
            or_panic!(stream.write_all(&[0]));
        }

        thread.join().unwrap();
    }

    #[test]
    fn long_path() {
        let dir = or_panic!(TempDir::new("unix_socket"));
        let socket_path = dir.path()
                             .join("asdfasdfasdfasdfasdfasdfasdfasdfasdfasdfasdfasdfasdfasdfasdfa\
                                    sasdfasdfasdasdfasdfasdfadfasdfasdfasdfasdfasdf");
        match UnixStream::connect(&socket_path) {
            Err(ref e) if e.kind() == io::ErrorKind::InvalidInput => {}
            Err(e) => panic!("unexpected error {}", e),
            Ok(_) => panic!("unexpected success"),
        }

        match UnixStreamListener::bind(&socket_path) {
            Err(ref e) if e.kind() == io::ErrorKind::InvalidInput => {}
            Err(e) => panic!("unexpected error {}", e),
            Ok(_) => panic!("unexpected success"),
        }

        match UnixDatagram::bind(&socket_path) {
            Err(ref e) if e.kind() == io::ErrorKind::InvalidInput => {}
            Err(e) => panic!("unexpected error {}", e),
            Ok(_) => panic!("unexpected success"),
        }
    }

    #[test]
    fn timeouts() {
        let dir = or_panic!(TempDir::new("unix_socket"));
        let socket_path = dir.path().join("sock");

        let _listener = or_panic!(UnixStreamListener::bind(&socket_path));

        let stream = or_panic!(UnixStream::connect(&socket_path));
        let dur = Duration::new(15410, 0);

        assert_eq!(None, or_panic!(stream.read_timeout()));

        or_panic!(stream.set_read_timeout(Some(dur)));
        assert_eq!(Some(dur), or_panic!(stream.read_timeout()));

        assert_eq!(None, or_panic!(stream.write_timeout()));

        or_panic!(stream.set_write_timeout(Some(dur)));
        assert_eq!(Some(dur), or_panic!(stream.write_timeout()));

        or_panic!(stream.set_read_timeout(None));
        assert_eq!(None, or_panic!(stream.read_timeout()));

        or_panic!(stream.set_write_timeout(None));
        assert_eq!(None, or_panic!(stream.write_timeout()));
    }

    #[test]
    fn test_read_timeout() {
        let dir = or_panic!(TempDir::new("unix_socket"));
        let socket_path = dir.path().join("sock");

        let _listener = or_panic!(UnixStreamListener::bind(&socket_path));

        let mut stream = or_panic!(UnixStream::connect(&socket_path));
        or_panic!(stream.set_read_timeout(Some(Duration::from_millis(1000))));

        let mut buf = [0; 10];
        let kind = stream.read(&mut buf).err().expect("expected error").kind();
        assert!(kind == io::ErrorKind::WouldBlock || kind == io::ErrorKind::TimedOut);
    }

    #[test]
    fn test_read_with_timeout() {
        let dir = or_panic!(TempDir::new("unix_socket"));
        let socket_path = dir.path().join("sock");

        let listener = or_panic!(UnixStreamListener::bind(&socket_path));

        let mut stream = or_panic!(UnixStream::connect(&socket_path));
        or_panic!(stream.set_read_timeout(Some(Duration::from_millis(1000))));

        let mut other_end = or_panic!(listener.accept()).0;
        or_panic!(other_end.write_all(b"hello world"));

        let mut buf = [0; 11];
        or_panic!(stream.read(&mut buf));
        assert_eq!(b"hello world", &buf[..]);

        let kind = stream.read(&mut buf).err().expect("expected error").kind();
        assert!(kind == io::ErrorKind::WouldBlock || kind == io::ErrorKind::TimedOut);
    }

    #[test]
    fn test_unix_datagram() {
        let dir = or_panic!(TempDir::new("unix_socket"));
        let path1 = dir.path().join("sock1");
        let path2 = dir.path().join("sock2");

        let sock1 = or_panic!(UnixDatagram::bind(&path1));
        let sock2 = or_panic!(UnixDatagram::bind(&path2));

        let msg = b"hello world";
        or_panic!(sock1.send_to(msg, &path2));
        let mut buf = [0; 11];
        or_panic!(sock2.recv_from(&mut buf));
        assert_eq!(msg, &buf[..]);
    }

    #[test]
    fn test_unnamed_unix_datagram() {
        let dir = or_panic!(TempDir::new("unix_socket"));
        let path1 = dir.path().join("sock1");

        let sock1 = or_panic!(UnixDatagram::bind(&path1));
        let sock2 = or_panic!(UnixDatagram::unbound());

        let msg = b"hello world";
        or_panic!(sock2.send_to(msg, &path1));
        let mut buf = [0; 11];
        let (usize, addr) = or_panic!(sock1.recv_from(&mut buf));
        assert_eq!(usize, 11);
        assert!(addr.is_unnamed());
        assert_eq!(msg, &buf[..]);
    }

    #[test]
    fn test_connect_unix_datagram() {
        let dir = or_panic!(TempDir::new("unix_socket"));
        let path1 = dir.path().join("sock1");
        let path2 = dir.path().join("sock2");

        let bsock1 = or_panic!(UnixDatagram::bind(&path1));
        let bsock2 = or_panic!(UnixDatagram::bind(&path2));
        let sock = or_panic!(UnixDatagram::unbound());
        or_panic!(sock.connect(&path1));

        // Check send()
        let msg = b"hello there";
        or_panic!(sock.send(msg));
        let mut buf = [0; 11];
        let (usize, addr) = or_panic!(bsock1.recv_from(&mut buf));
        assert_eq!(usize, 11);
        assert!(addr.is_unnamed());
        assert_eq!(msg, &buf[..]);

        // Changing default socket works too
        or_panic!(sock.connect(&path2));
        or_panic!(sock.send(msg));
        or_panic!(bsock2.recv_from(&mut buf));
    }

    #[test]
    fn test_unix_datagram_recv() {
        let dir = or_panic!(TempDir::new("unix_socket"));
        let path1 = dir.path().join("sock1");

        let sock1 = or_panic!(UnixDatagram::bind(&path1));
        let sock2 = or_panic!(UnixDatagram::unbound());
        or_panic!(sock2.connect(&path1));

        let msg = b"hello world";
        or_panic!(sock2.send(msg));
        let mut buf = [0; 11];
        let size = or_panic!(sock1.recv(&mut buf));
        assert_eq!(size, 11);
        assert_eq!(msg, &buf[..]);
    }

    #[test]
    fn datagram_pair() {
        let msg1 = b"hello";
        let msg2 = b"world!";

        let (s1, s2) = or_panic!(UnixDatagram::pair());
        let thread = thread::spawn(move || {
            // s1 must be moved in or the test will hang!
            let mut buf = [0; 5];
            or_panic!(s1.recv(&mut buf));
            assert_eq!(&msg1[..], &buf[..]);
            or_panic!(s1.send(msg2));
        });

        or_panic!(s2.send(msg1));
        let mut buf = [0; 6];
        or_panic!(s2.recv(&mut buf));
        assert_eq!(&msg2[..], &buf[..]);
        drop(s2);

        thread.join().unwrap();
    }

    #[test]
    fn datagram_shutdown() {
        let s1 = UnixDatagram::unbound().unwrap();
        let s2 = s1.try_clone().unwrap();

        let thread = thread::spawn(move || {
            let mut buf = [0; 1];
            assert_eq!(0, s1.recv_from(&mut buf).unwrap().0);
        });

        thread::sleep(Duration::from_millis(100));
        s2.shutdown(Shutdown::Read).unwrap();;

        thread.join().unwrap();
    }
}
