use crate::lwip::{
    err_enum_t_ERR_CONN, err_enum_t_ERR_OK, err_t, ip_addr_any_type, tcp_accept, tcp_arg, tcp_bind,
    tcp_close, tcp_listen_with_backlog_and_err, tcp_new, tcp_pcb, TCP_DEFAULT_LISTEN_BACKLOG,
};
use crate::tcp_stream::TcpStream;
use crate::LWIP_MUTEX;
use std::io;
use std::os::raw;
use tokio::sync::mpsc::{channel, Receiver, Sender};

#[allow(unused_variables)]
pub extern "C" fn tcp_accept_cb(arg: *mut raw::c_void, new_pcb: *mut tcp_pcb, err: err_t) -> err_t {
    if arg.is_null() {
        log::warn!("tcp listener has been closed");
        return err_enum_t_ERR_CONN as err_t;
    }
    if new_pcb.is_null() {
        log::warn!("tcp full");
        return err_enum_t_ERR_OK as err_t;
    }
    if err != err_enum_t_ERR_OK as err_t {
        log::warn!("accept tcp failed: {}", err);
        // Not sure what to do if there was an error, just ignore it.
        return err_enum_t_ERR_OK as err_t;
    }
    let sender = unsafe { &*(arg as *mut Sender<TcpStream>) };
    let stream = TcpStream::new(new_pcb);
    if sender.try_send(stream).is_err() {
        log::warn!("tcp channel full");
        return err_enum_t_ERR_CONN as err_t;
    }
    err_enum_t_ERR_OK as err_t
}

pub struct TcpListener {
    receiver: Receiver<TcpStream>,
    sender: *mut Sender<TcpStream>,
    pcb: *mut tcp_pcb,
}
unsafe impl Send for TcpListener {}

unsafe impl Sync for TcpListener {}

impl TcpListener {
    pub fn new() -> io::Result<Self> {
        let _g = LWIP_MUTEX.lock();
        unsafe {
            let mut pcb = tcp_new();
            let err = tcp_bind(pcb, &ip_addr_any_type, 0);
            if err != err_enum_t_ERR_OK as err_t {
                return Err(io::Error::new(
                    io::ErrorKind::Other,
                    format!("bind TCP failed: {}", err),
                ));
            }
            let mut reason: err_t = 0;
            pcb =
                tcp_listen_with_backlog_and_err(pcb, TCP_DEFAULT_LISTEN_BACKLOG as u8, &mut reason);
            if pcb.is_null() {
                return Err(io::Error::new(
                    io::ErrorKind::Other,
                    format!("listen TCP failed: {}", reason),
                ));
            }
            let (sender, receiver) = channel(255);
            let sender = Box::into_raw(Box::new(sender));
            tcp_arg(pcb, sender as _);
            tcp_accept(pcb, Some(tcp_accept_cb));
            Ok(TcpListener {
                receiver,
                sender,
                pcb,
            })
        }
    }
    pub async fn accept(&mut self) -> io::Result<TcpStream> {
        self.receiver
            .recv()
            .await
            .ok_or_else(|| io::Error::new(io::ErrorKind::UnexpectedEof, "close"))
    }
}

impl Drop for TcpListener {
    fn drop(&mut self) {
        let _g = LWIP_MUTEX.lock();
        unsafe {
            tcp_arg(self.pcb, std::ptr::null_mut());
            tcp_accept(self.pcb, None);
            tcp_close(self.pcb);
            let _ = Box::from_raw(self.sender);
        }
    }
}
