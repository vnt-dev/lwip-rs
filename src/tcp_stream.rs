use crate::lwip::{
    err_enum_t_ERR_MEM, err_enum_t_ERR_OK, err_t, pbuf, pbuf_copy_partial, pbuf_free, tcp_arg,
    tcp_bind, tcp_connect, tcp_err, tcp_new, tcp_output, tcp_pcb, tcp_poll, tcp_recv, tcp_recved,
    tcp_sent, tcp_shutdown, tcp_write, u16_t, SOF_KEEPALIVE, TCP_WRITE_FLAG_COPY, TF_NODELAY,
};
use crate::{util, LWIP_MUTEX};
use std::io;
use std::io::Error;
use std::net::SocketAddr;
use std::os::raw;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll, Waker};
use std::time::Duration;
use tokio::io::{AsyncRead, ReadBuf};
use tokio::sync::mpsc::error::TrySendError;
use tokio::sync::mpsc::{channel, Receiver, Sender};
use tokio::sync::watch;

#[allow(unused_variables)]
unsafe extern "C" fn tcp_connect_cb(arg: *mut raw::c_void, pcb: *mut tcp_pcb, err: err_t) -> err_t {
    if arg.is_null() {
        log::warn!("tcp_connect_cb tcp connection has been closed");
        return err_enum_t_ERR_OK as err_t;
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
    t_pcb: *mut tcp_pcb,
    p: *mut pbuf,
    err: err_t,
) -> err_t {
    if arg.is_null() {
        log::warn!("tcp_recv_cb arg.is_null()");
        return err_enum_t_ERR_OK as err_t;
    }
    let arg = arg as *mut TcpArgs;
    if p.is_null() {
        log::info!("tcp_recv_cb p.is_null ");
        (&*arg).close_read();
        return err_enum_t_ERR_OK as err_t;
    }

    if let Err(e) = (&*arg).buf_sender.try_send(ReadItem::new(t_pcb, p)) {
        match e {
            TrySendError::Full(read_item) => {
                //不回收数据
                read_item.back();
                log::warn!("tcp read busy")
            }
            TrySendError::Closed(_) => {
                log::warn!("tcp close")
            }
        }
    }
    err_enum_t_ERR_OK as err_t
}

#[allow(unused_variables)]
pub unsafe extern "C" fn tcp_sent_cb(
    arg: *mut raw::c_void,
    tpcb: *mut tcp_pcb,
    len: u16_t,
) -> err_t {
    if arg.is_null() {
        log::warn!("tcp_sent_cb arg.is_null()");
        return err_enum_t_ERR_OK as err_t;
    }
    let arg = arg as *mut TcpArgs;
    if let Some(w) = (*arg).notify.take() {
        w.wake()
    }
    err_enum_t_ERR_OK as err_t
}

#[allow(unused_variables)]
pub unsafe extern "C" fn tcp_poll_cb(arg: *mut raw::c_void, tpcb: *mut tcp_pcb) -> err_t {
    if arg.is_null() {
        log::warn!("tcp_sent_cb arg.is_null()");
        return err_enum_t_ERR_OK as err_t;
    }
    let arg = arg as *mut TcpArgs;
    if let Some(w) = (*arg).notify.take() {
        w.wake()
    }
    err_enum_t_ERR_OK as err_t
}

#[allow(unused_variables)]
pub unsafe extern "C" fn tcp_err_cb(arg: *mut raw::c_void, err: err_t) {
    log::warn!("tcp_err_cb tcp connection has been closed");
    if arg.is_null() {
        return;
    }
    let arg = arg as *mut TcpArgs;
    (&*arg).close();
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
            // if tcp_close(self.pcb) != err_enum_t_ERR_OK as _ {
            //     tcp_abort(self.pcb);
            // }
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
    buf_sender: Sender<ReadItem>,
    state_sender: watch::Sender<usize>,
    notify: Option<Waker>,
}

impl TcpArgs {
    pub fn close(&self) {
        let _ = self.state_sender.send(CLOSE);
        let _ = self.buf_sender.try_send(ReadItem::new_null());
    }
    pub fn close_read(&self) {
        let _ = self.buf_sender.try_send(ReadItem::new_null());
    }
}

const SYN_SENT: usize = 0;
const ESTABLISHED: usize = 1;
const CLOSE: usize = 2;

pub struct ReadItem {
    t_pcb: *mut tcp_pcb,
    p: *mut pbuf,
    offset: usize,
}

impl ReadItem {
    pub fn new_null() -> Self {
        Self {
            t_pcb: std::ptr::null_mut(),
            p: std::ptr::null_mut(),
            offset: 0,
        }
    }
    pub fn new(t_pcb: *mut tcp_pcb, p: *mut pbuf) -> Self {
        Self {
            t_pcb,
            p,
            offset: 0,
        }
    }
    pub fn is_empty(&self) -> bool {
        if self.p.is_null() {
            return true;
        }
        unsafe {
            let pbuflen = std::ptr::read_unaligned(self.p).tot_len;
            pbuflen == self.offset as _
        }
    }
    pub fn len(&self) -> usize {
        if self.p.is_null() {
            return 0;
        }
        unsafe {
            let pbuflen = std::ptr::read_unaligned(self.p).tot_len as usize;
            pbuflen - self.offset
        }
    }
    pub fn read(&mut self, buf: &mut [u8]) -> usize {
        unsafe {
            let len = std::ptr::read_unaligned(self.p).tot_len as usize - self.offset;
            let pbuflen = buf.len().min(len);
            if pbuflen == 0 {
                return 0;
            }
            pbuf_copy_partial(
                self.p,
                buf.as_mut_ptr() as _,
                pbuflen as _,
                self.offset as _,
            );
            self.offset += pbuflen;
            pbuflen
        }
    }
    pub fn recved(mut self) {
        unsafe {
            pbuf_free(self.p);
            let pbuflen = std::ptr::read_unaligned(self.p).tot_len;
            tcp_recved(self.t_pcb, pbuflen);
            self.p = std::ptr::null_mut();
        }
    }
    pub fn back(mut self) {
        self.p = std::ptr::null_mut();
        self.t_pcb = std::ptr::null_mut();
    }
}

impl Drop for ReadItem {
    fn drop(&mut self) {
        if !self.p.is_null() {
            let _g = LWIP_MUTEX.lock();
            unsafe {
                pbuf_free(self.p);
            }
        }
    }
}

unsafe impl Send for ReadItem {}

impl TcpStream {
    pub(crate) fn new(pcb: *mut tcp_pcb) -> TcpStream {
        let _g = LWIP_MUTEX.lock();
        let (state_sender, _state_receiver) = watch::channel(ESTABLISHED);
        let (buf_sender, buf_receiver) = channel(1024);
        let tcp_args = Box::into_raw(Box::new(TcpArgs {
            buf_sender,
            state_sender,
            notify: None,
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
                last: None,
            };

            tcp_arg(pcb, tcp_args as *mut raw::c_void);
            tcp_err(pcb, Some(tcp_err_cb));
            tcp_recv(pcb, Some(tcp_recv_cb));
            tcp_sent(pcb, Some(tcp_sent_cb));
            // tcp_poll(pcb, Some(tcp_poll_cb), 8 as _);
            let mut pcb_v = std::ptr::read_unaligned(pcb as *const tcp_pcb);
            pcb_v.snd_buf = 65535;
            pcb_v.so_options |= SOF_KEEPALIVE as u8;
            pcb_v.flags |= TF_NODELAY as u16;
            std::ptr::write_unaligned(pcb, pcb_v);
            TcpStream { write, read }
        }
    }
    pub async fn connect(
        mut src_addr: SocketAddr,
        dest_addr: SocketAddr,
        time: Duration,
    ) -> io::Result<TcpStream> {
        let (state_sender, mut state_receiver) = watch::channel(SYN_SENT);

        let (write, read) = unsafe {
            let _g = LWIP_MUTEX.lock();
            let pcb = tcp_new();
            if pcb.is_null() {
                return Err(Error::new(io::ErrorKind::Other, "tcp_new failed: is_null"));
            }
            let err = tcp_bind(pcb, &util::to_ip_addr_t(src_addr.ip()), src_addr.port());
            if err != err_enum_t_ERR_OK as err_t {
                return Err(Error::new(
                    io::ErrorKind::Other,
                    format!("bind TCP failed: {} {}", src_addr, err),
                ));
            }
            {
                let pcb_v = std::ptr::read_unaligned(pcb);
                src_addr = util::to_socket_addr(&pcb_v.local_ip, pcb_v.local_port);
            }
            let (buf_sender, buf_receiver) = channel(1024);
            let tcp_args = Box::into_raw(Box::new(TcpArgs {
                buf_sender,
                state_sender,
                notify: None,
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
                last: None,
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
            tcp_sent(pcb, Some(tcp_sent_cb));
            // tcp_poll(pcb, Some(tcp_poll_cb), 8 as _);

            let mut pcb_v = std::ptr::read_unaligned(pcb as *const tcp_pcb);
            pcb_v.snd_buf = 65535;
            pcb_v.so_options |= SOF_KEEPALIVE as u8;
            pcb_v.flags |= TF_NODELAY as u16;
            std::ptr::write_unaligned(pcb, pcb_v);
            (write, read)
        };

        match tokio::time::timeout(time, state_receiver.changed()).await {
            Ok(rs) => {
                if rs.is_err() || *state_receiver.borrow() == CLOSE {
                    return Err(Error::new(
                        io::ErrorKind::ConnectionRefused,
                        "ConnectionRefused",
                    ));
                }
            }
            Err(_) => {
                return Err(Error::new(io::ErrorKind::TimedOut, "TimedOut"));
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
    receiver: Receiver<ReadItem>,
    tcp_context: Arc<TcpContext>,
    last: Option<ReadItem>,
}

impl AsyncRead for TcpStreamRead {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context,
        buf: &mut ReadBuf,
    ) -> Poll<io::Result<()>> {
        let _guard = LWIP_MUTEX.lock();
        if self.tcp_context.is_close() {
            return Poll::Ready(Err(Error::new(io::ErrorKind::UnexpectedEof, "close")));
        }
        if let Some(last) = &mut self.last {
            buf.advance(buf.remaining().min(last.len()));
            last.read(buf.filled_mut());
            if last.is_empty() {
                if let Some(last) = self.last.take() {
                    last.recved();
                }
            }
            return Poll::Ready(Ok(()));
        }
        return match Pin::new(&mut self.receiver).poll_recv(cx) {
            Poll::Ready(read_item) => {
                if let Some(mut read_item) = read_item {
                    if !read_item.is_empty() {
                        buf.advance(buf.remaining().min(read_item.len()));
                        read_item.read(buf.filled_mut());
                        if !read_item.is_empty() {
                            self.last.replace(read_item);
                        } else {
                            read_item.recved();
                        }
                        return Poll::Ready(Ok(()));
                    }
                }
                Poll::Ready(Err(Error::new(io::ErrorKind::UnexpectedEof, "close")))
            }
            Poll::Pending => Poll::Pending,
        };
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
    fn send_buf_size(&self) -> usize {
        unsafe { std::ptr::read_volatile(self.tcp_context.pcb as *const tcp_pcb).snd_buf as usize }
    }
}

impl tokio::io::AsyncWrite for TcpStreamWrite {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<Result<usize, Error>> {
        let _guard = LWIP_MUTEX.lock();
        if self.tcp_context.is_close() {
            return Poll::Ready(Err(Error::new(io::ErrorKind::UnexpectedEof, "close")));
        }
        let to_write = buf.len().min(self.send_buf_size());
        if to_write == 0 {
            unsafe {
                (*self.tcp_context.tcp_args)
                    .notify
                    .replace(cx.waker().clone());
            }
            return Poll::Pending;
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
            // Call output in case of mem err?
            let err = unsafe { tcp_output(self.tcp_context.pcb) };
            if err == err_enum_t_ERR_OK as err_t {
                Poll::Ready(Ok(to_write))
            } else {
                Poll::Ready(Err(Error::new(
                    io::ErrorKind::Interrupted,
                    format!("netstack tcp_output error {}", err),
                )))
            }
        } else if err == err_enum_t_ERR_MEM as err_t {
            unsafe {
                (*self.tcp_context.tcp_args)
                    .notify
                    .replace(cx.waker().clone());
            }
            Poll::Pending
        } else {
            Poll::Ready(Err(Error::new(
                io::ErrorKind::Interrupted,
                format!("netstack tcp_write error {}", err),
            )))
        }
    }

    fn poll_flush(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<Result<(), Error>> {
        let _guard = LWIP_MUTEX.lock();
        if self.tcp_context.is_close() {
            return Poll::Ready(Err(Error::new(io::ErrorKind::UnexpectedEof, "close")));
        }
        let err = unsafe { tcp_output(self.tcp_context.pcb) };
        if err != err_enum_t_ERR_OK as err_t {
            Poll::Ready(Err(Error::new(
                io::ErrorKind::Interrupted,
                format!("netstack tcp_output error {}", err),
            )))
        } else {
            Poll::Ready(Ok(()))
        }
    }

    fn poll_shutdown(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<Result<(), Error>> {
        let _guard = LWIP_MUTEX.lock();
        let err = unsafe { tcp_shutdown(self.tcp_context.pcb, 0, 1) };
        if err != err_enum_t_ERR_OK as err_t {
            Poll::Ready(Err(Error::new(
                io::ErrorKind::Interrupted,
                format!("netstack tcp_shutdown tx error {}", err),
            )))
        } else {
            Poll::Ready(Ok(()))
        }
    }
}
