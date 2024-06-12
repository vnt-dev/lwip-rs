use std::io;
use std::net::SocketAddr;
use std::ops::Deref;
use std::os::raw;
use std::sync::Arc;

use tokio::sync::mpsc::{channel, Receiver, Sender};

use crate::{util, LWIP_MUTEX};

use super::lwip::*;

#[allow(unused_variables)]
pub unsafe extern "C" fn udp_recv_cb(
    arg: *mut raw::c_void,
    pcb: *mut udp_pcb,
    p: *mut pbuf,
    addr: *const ip_addr_t,
    port: u16_t,
    dst_addr: *const ip_addr_t,
    dst_port: u16_t,
) {
    if arg.is_null() {
        log::warn!("udp socket has been closed");
        return;
    }
    let socket = &mut *(arg as *mut UdpSocket);
    let src_addr = util::to_socket_addr(&*addr, port);
    let dst_addr = util::to_socket_addr(&*dst_addr, dst_port);
    let tot_len = std::ptr::read_unaligned(p).tot_len;
    let mut buf: Vec<u8> = Vec::with_capacity(tot_len as usize);
    pbuf_copy_partial(p, buf.as_mut_ptr() as *mut _, tot_len, 0);
    buf.set_len(tot_len as usize);
    let arg = arg as *mut Sender<(Vec<u8>, SocketAddr, SocketAddr)>;
    if let Err(_) = (&*arg).try_send((buf, src_addr, dst_addr)) {
        log::warn!("ip mapping packet loss")
    }
    pbuf_free(p);
}

fn send_udp(
    src_addr: &SocketAddr,
    dst_addr: &SocketAddr,
    pcb: *mut udp_pcb,
    data: &[u8],
) -> io::Result<()> {
    let len = data.len();
    if len == 0 {
        return Ok(());
    }
    let _g = LWIP_MUTEX.lock();
    unsafe {
        let pbuf = pbuf_alloc_reference(data.as_ptr() as *mut _, len as _, pbuf_type_PBUF_REF);
        if pbuf.is_null() {
            return Err(io::Error::new(io::ErrorKind::WouldBlock, ""));
        }
        let src_ip = util::to_ip_addr_t(src_addr.ip());
        let dst_ip = util::to_ip_addr_t(dst_addr.ip());
        let err = udp_sendto(
            pcb,
            pbuf,
            &dst_ip as *const _,
            dst_addr.port(),
            &src_ip as *const _,
            src_addr.port(),
        );
        pbuf_free(pbuf);
        if err != err_enum_t_ERR_OK as err_t {
            return Err(io::Error::new(
                io::ErrorKind::Other,
                format!("udp_sendto error: {}", err),
            ));
        }
        Ok(())
    }
}

pub struct UdpSocket {
    write: UdpSocketWrite,
    read: UdpSocketRead,
}

impl UdpSocket {
    pub fn new() -> io::Result<Self> {
        let (write, read) = new0()?;
        Ok(Self { write, read })
    }
    pub async fn recv(&mut self) -> io::Result<(Vec<u8>, SocketAddr, SocketAddr)> {
        self.read.recv().await
    }
    pub fn send(&self, buf: &[u8], src_addr: &SocketAddr, dst_addr: &SocketAddr) -> io::Result<()> {
        self.write.send(buf, src_addr, dst_addr)
    }
    pub fn into_split(self) -> (UdpSocketWrite, UdpSocketRead) {
        (self.write, self.read)
    }
}

pub struct UdpSocketRead {
    receiver: Receiver<(Vec<u8>, SocketAddr, SocketAddr)>,
}

#[derive(Clone)]
pub struct UdpSocketWrite {
    inner: Arc<UdpSocketWriteInner>,
}

impl Deref for UdpSocketWrite {
    type Target = UdpSocketWriteInner;

    fn deref(&self) -> &Self::Target {
        &self.inner
    }
}

pub struct UdpSocketWriteInner {
    udp_pcb: *mut udp_pcb,
    arg: *mut Sender<(Vec<u8>, SocketAddr, SocketAddr)>,
}

unsafe impl Send for UdpSocketWriteInner {}

unsafe impl Sync for UdpSocketWriteInner {}

fn new0() -> io::Result<(UdpSocketWrite, UdpSocketRead)> {
    let _g = LWIP_MUTEX.lock();
    unsafe {
        let udp_pcb = udp_new();
        if udp_pcb.is_null() {
            return Err(io::Error::new(
                io::ErrorKind::Other,
                "udp_new failed: is_null",
            ));
        }
        let err = udp_bind(udp_pcb, &ip_addr_any_type, 0);
        if err != err_enum_t_ERR_OK as err_t {
            return Err(io::Error::new(
                io::ErrorKind::Other,
                format!("bind UDP failed: {}", err),
            ));
        }
        let (sender, receiver) = channel(1024);
        let arg = Box::into_raw(Box::new(sender));
        udp_recv(udp_pcb, Some(udp_recv_cb), arg as *mut raw::c_void);
        Ok((
            UdpSocketWrite {
                inner: Arc::new(UdpSocketWriteInner { udp_pcb, arg }),
            },
            UdpSocketRead { receiver },
        ))
    }
}

impl UdpSocketWriteInner {
    pub fn send(&self, buf: &[u8], src_addr: &SocketAddr, dst_addr: &SocketAddr) -> io::Result<()> {
        send_udp(src_addr, dst_addr, self.udp_pcb, buf)
    }
}

impl UdpSocketRead {
    pub async fn recv(&mut self) -> io::Result<(Vec<u8>, SocketAddr, SocketAddr)> {
        self.receiver
            .recv()
            .await
            .ok_or_else(|| io::Error::new(io::ErrorKind::UnexpectedEof, "close"))
    }
}

impl Drop for UdpSocketWriteInner {
    fn drop(&mut self) {
        let _g = LWIP_MUTEX.lock();
        unsafe {
            udp_recv(self.udp_pcb, None, std::ptr::null_mut());
            udp_disconnect(self.udp_pcb);
            udp_remove(self.udp_pcb);
            drop(Box::from_raw(self.arg));
        }
    }
}
