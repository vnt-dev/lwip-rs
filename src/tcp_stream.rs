use crate::lwip::{
    err_enum_t_ERR_CONN, err_enum_t_ERR_OK, err_t, pbuf, pbuf_copy_partial, pbuf_free, tcp_abort,
    tcp_arg, tcp_bind, tcp_connect, tcp_err, tcp_new, tcp_output, tcp_pcb, tcp_poll, tcp_recv,
    tcp_sent, tcp_shutdown, tcp_write, u16_t, SOF_KEEPALIVE, TCP_WRITE_FLAG_COPY, TF_NODELAY,
};
use crate::{util, LWIP_MUTEX};
use std::io;
use std::net::SocketAddr;
use std::os::raw;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::mpsc::{channel, Receiver, Sender};
use tokio::sync::watch;

#[allow(unused_variables)]
unsafe extern "C" fn tcp_connect_cb(arg: *mut raw::c_void, pcb: *mut tcp_pcb, err: err_t) -> err_t {
    if arg.is_null() {
        log::warn!("tcp_connect_cb tcp connection has been closed");
        return err_enum_t_ERR_CONN as err_t;
    }
    if err == err_enum_t_ERR_OK as _ {
        let arg = arg as *mut TcpArgs;
        let _ = (&*arg).state_sender.send(ESTABLISHED);
        return err;
    }
    let arg = arg as *mut TcpArgs;
    let _ = (&*arg).state_sender.send(CLOSE);
    err_enum_t_ERR_OK as _
}

#[allow(unused_variables)]
pub unsafe extern "C" fn tcp_recv_cb(
    arg: *mut raw::c_void,
    tpcb: *mut tcp_pcb,
    p: *mut pbuf,
    err: err_t,
) -> err_t {
    if arg.is_null() {
        log::warn!("tcp_recv_cb arg.is_null()");
        return err_enum_t_ERR_CONN as err_t;
    }
    let arg = arg as *mut TcpArgs;
    if p.is_null() || err != err_enum_t_ERR_OK as _ {
        log::info!("tcp_recv_cb p.is_null ");
        let _ = (&*arg).state_sender.send(CLOSE);
        if let Err(_) = (&*arg).buf_sender.try_send(vec![]) {
            log::warn!("ip mapping packet loss")
        }
        return err_enum_t_ERR_CONN as err_t;
    }

    let pbuflen = std::ptr::read_unaligned(p).tot_len;
    let mut buf = Vec::with_capacity(pbuflen as usize);
    pbuf_copy_partial(p, buf.as_mut_ptr() as _, pbuflen, 0);
    buf.set_len(pbuflen as usize);

    if let Err(_) = (&*arg).buf_sender.try_send(buf) {
        log::warn!("ip mapping packet loss")
    }

    pbuf_free(p);
    err_enum_t_ERR_OK as err_t
}

#[allow(unused_variables)]
pub unsafe extern "C" fn tcp_err_cb(arg: *mut raw::c_void, err: err_t) {
    if arg.is_null() {
        log::warn!("tcp_err_cb tcp connection has been closed");
        return;
    }
    let arg = arg as *mut TcpArgs;
    let _ = (&*arg).state_sender.send(CLOSE);
    let _ = (&*arg).buf_sender.send(vec![]);
}

pub struct TcpStream {
    write: TcpStreamWrite,
    read: TcpStreamRead,
}

pub struct TcpContext {
    pcb: *mut tcp_pcb,
    tcp_args: *mut TcpArgs,
}

unsafe impl Send for TcpContext {}

unsafe impl Sync for TcpContext {}

impl Drop for TcpStreamWrite {
    fn drop(&mut self) {
        let _g = LWIP_MUTEX.lock();
        unsafe {
            tcp_shutdown(self.tcp_context.pcb, 0, 1);
        }
    }
}

impl Drop for TcpStreamRead {
    fn drop(&mut self) {
        let _g = LWIP_MUTEX.lock();
        unsafe {
            tcp_shutdown(self.tcp_context.pcb, 1, 0);
        }
    }
}

impl Drop for TcpContext {
    fn drop(&mut self) {
        let _g = LWIP_MUTEX.lock();
        unsafe {
            tcp_arg(self.pcb, std::ptr::null_mut());
            tcp_recv(self.pcb, None);
            tcp_sent(self.pcb, None);
            tcp_err(self.pcb, None);
            tcp_poll(self.pcb, None, 0);
            tcp_abort(self.pcb);
            let _ = Box::from_raw(self.tcp_args);
        }
    }
}

impl TcpContext {
    pub fn is_close(&self) -> bool {
        unsafe { *(*self.tcp_args).state_sender.borrow() != ESTABLISHED }
    }
}

pub struct TcpArgs {
    buf_sender: Sender<Vec<u8>>,
    state_sender: watch::Sender<usize>,
}

const SYN_SENT: usize = 0;
const ESTABLISHED: usize = 1;
const CLOSE: usize = 2;

impl TcpStream {
    pub(crate) fn new(pcb: *mut tcp_pcb) -> TcpStream {
        let (state_sender, _state_receiver) = watch::channel(ESTABLISHED);
        let (buf_sender, buf_receiver) = channel(1024);
        let tcp_args = Box::into_raw(Box::new(TcpArgs {
            buf_sender,
            state_sender,
        }));
        unsafe {
            let pcb_v = std::ptr::read_unaligned(pcb);
            let src_addr = util::to_socket_addr(&pcb_v.remote_ip, pcb_v.remote_port);
            let dest_addr = util::to_socket_addr(&pcb_v.local_ip, pcb_v.local_port);
            let tcp_context = Arc::new(TcpContext { pcb, tcp_args });
            let write = TcpStreamWrite {
                src_addr,
                dest_addr,
                tcp_context: tcp_context.clone(),
            };
            let read = TcpStreamRead {
                receiver: buf_receiver,
                tcp_context,
            };

            tcp_arg(pcb, tcp_args as *mut raw::c_void);
            tcp_err(pcb, Some(tcp_err_cb));
            tcp_recv(pcb, Some(tcp_recv_cb));
            let mut pcb_v = std::ptr::read_unaligned(pcb as *const tcp_pcb);
            pcb_v.so_options |= SOF_KEEPALIVE as u8;
            pcb_v.flags |= TF_NODELAY as u16;
            std::ptr::write_unaligned(pcb, pcb_v);
            TcpStream { write, read }
        }
    }
    pub async fn connect(
        src_addr: SocketAddr,
        dest_addr: SocketAddr,
        time: Duration,
    ) -> io::Result<TcpStream> {
        let (state_sender, mut state_receiver) = watch::channel(SYN_SENT);

        let (write, read) = unsafe {
            let _g = LWIP_MUTEX.lock();
            let pcb = tcp_new();
            let err = tcp_bind(pcb, &util::to_ip_addr_t(src_addr.ip()), src_addr.port());
            if err != err_enum_t_ERR_OK as err_t {
                return Err(io::Error::new(
                    io::ErrorKind::Other,
                    format!("bind TCP failed: {} {}", src_addr, err),
                ));
            }
            let (buf_sender, buf_receiver) = channel(1024);
            let tcp_args = Box::into_raw(Box::new(TcpArgs {
                buf_sender,
                state_sender,
            }));
            let tcp_context = Arc::new(TcpContext { pcb, tcp_args });
            let write = TcpStreamWrite {
                src_addr,
                dest_addr,
                tcp_context: tcp_context.clone(),
            };
            let read = TcpStreamRead {
                receiver: buf_receiver,
                tcp_context,
            };

            tcp_arg(pcb, tcp_args as *mut raw::c_void);
            tcp_err(pcb, Some(tcp_err_cb));
            let dest_ip = util::to_ip_addr_t(dest_addr.ip());
            let err = tcp_connect(pcb, &dest_ip, dest_addr.port(), Some(tcp_connect_cb));
            if err != err_enum_t_ERR_OK as err_t {
                return Err(io::Error::new(
                    io::ErrorKind::Other,
                    format!("listen TCP failed: {}", err),
                ));
            }

            tcp_recv(pcb, Some(tcp_recv_cb));
            let mut pcb_v = std::ptr::read_unaligned(pcb as *const tcp_pcb);
            pcb_v.so_options |= SOF_KEEPALIVE as u8;
            pcb_v.flags |= TF_NODELAY as u16;
            std::ptr::write_unaligned(pcb, pcb_v);
            (write, read)
        };

        match tokio::time::timeout(time, state_receiver.changed()).await {
            Ok(rs) => {
                if rs.is_err() || *state_receiver.borrow() == CLOSE {
                    return Err(io::Error::new(
                        io::ErrorKind::ConnectionRefused,
                        "ConnectionRefused",
                    ));
                }
            }
            Err(_) => {
                return Err(io::Error::new(io::ErrorKind::TimedOut, "TimedOut"));
            }
        }

        Ok(TcpStream { write, read })
    }
    pub fn into_split(self) -> (TcpStreamWrite, TcpStreamRead) {
        (self.write, self.read)
    }
    pub fn src_addr(&self) -> SocketAddr {
        self.write.src_addr()
    }
    pub fn dest_addr(&self) -> SocketAddr {
        self.write.dest_addr()
    }
}

pub struct TcpStreamRead {
    receiver: Receiver<Vec<u8>>,
    tcp_context: Arc<TcpContext>,
}

impl TcpStreamRead {
    pub async fn read(&mut self) -> io::Result<Vec<u8>> {
        if self.tcp_context.is_close() {
            return Err(io::Error::new(io::ErrorKind::UnexpectedEof, "close"));
        }
        if let Some(buf) = self.receiver.recv().await {
            if !buf.is_empty() {
                return Ok(buf);
            }
        }
        Err(io::Error::new(io::ErrorKind::UnexpectedEof, "close"))
    }
}

pub struct TcpStreamWrite {
    src_addr: SocketAddr,
    dest_addr: SocketAddr,
    tcp_context: Arc<TcpContext>,
}

impl TcpStreamWrite {
    pub fn src_addr(&self) -> SocketAddr {
        self.src_addr
    }
    pub fn dest_addr(&self) -> SocketAddr {
        self.dest_addr
    }
    pub fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        if self.tcp_context.is_close() {
            return Err(io::Error::new(io::ErrorKind::UnexpectedEof, "close"));
        }
        let _g = LWIP_MUTEX.lock();
        let to_write = buf.len().min(self.send_buf_size());
        if to_write == 0 {
            return Err(io::Error::new(io::ErrorKind::WriteZero, ""));
        }
        let err = unsafe {
            tcp_write(
                self.tcp_context.pcb,
                buf.as_ptr() as *const raw::c_void,
                to_write as u16_t,
                TCP_WRITE_FLAG_COPY as u8,
            )
        };
        if err == err_enum_t_ERR_OK as err_t {
            let err = unsafe { tcp_output(self.tcp_context.pcb) };
            if err == err_enum_t_ERR_OK as err_t {
                Ok(to_write)
            } else {
                Err(io::Error::new(
                    io::ErrorKind::Interrupted,
                    format!("netstack tcp_output error {}", err),
                ))
            }
        } else {
            Err(io::Error::new(
                io::ErrorKind::Other,
                format!("netstack tcp_write error {}", err),
            ))
        }
    }
    pub async fn write_all(&mut self, mut buf: &[u8]) -> io::Result<()> {
        loop {
            let len = self.write(buf)?;
            if len == 0 {
                return Err(io::Error::new(io::ErrorKind::WriteZero, ""));
            }
            buf = &buf[len..];
            if buf.is_empty() {
                return Ok(());
            } else {
                //先等待再说
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
        }
    }
    fn send_buf_size(&self) -> usize {
        unsafe { std::ptr::read_unaligned(self.tcp_context.pcb as *const tcp_pcb).snd_buf as usize }
    }
}
