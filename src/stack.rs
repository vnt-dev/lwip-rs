use crossbeam::atomic::AtomicCell;
use lazy_static::lazy_static;
use parking_lot::Mutex;
use std::fmt::{Debug, Formatter};
use std::os::raw;
use std::sync::Once;
use std::{io, time};
use tokio::sync::mpsc::{channel, Receiver, Sender};
use tokio::task::JoinHandle;

use crate::lwip::*;
use crate::{util, LWIP_MUTEX};

#[allow(unused_variables)]
pub unsafe extern "C" fn output(netif: *mut netif, p: *mut pbuf) -> err_t {
    if p.is_null() {
        log::error!("output_ip4 p null");
        return err_enum_t_ERR_OK as _;
    }
    let pbuflen = std::ptr::read_unaligned(p).tot_len;
    let head_reserved = HEAD_RESERVED.load();
    let tail_reserved = TAIL_RESERVED.load();
    let len = pbuflen as usize + head_reserved + tail_reserved;
    let mut buf = Vec::with_capacity(len);
    let mut ptr: *mut u8 = buf.as_mut_ptr();
    ptr = ptr.add(head_reserved);
    pbuf_copy_partial(p, ptr as *mut _, pbuflen, 0);
    buf.set_len(len);
    buf[..head_reserved].fill(0);
    if let Some(v) = NET_STACK_WRITE.lock().as_ref() {
        if let Err(_) = v.try_send((buf, head_reserved, head_reserved + pbuflen as usize)) {
            log::error!("mapping output_ip4 channel close")
        }
    }
    err_enum_t_ERR_OK as _
}

#[allow(unused_variables)]
pub unsafe extern "C" fn output_ip4(
    netif: *mut netif,
    p: *mut pbuf,
    ipaddr: *const ip4_addr_t,
) -> err_t {
    output(netif, p)
}

lazy_static! {
    static ref HEAD_RESERVED: AtomicCell<usize> = AtomicCell::<usize>::new(0);
    static ref TAIL_RESERVED: AtomicCell<usize> = AtomicCell::<usize>::new(0);
    static ref NET_STACK_WRITE: Mutex<Option<Sender<(Vec<u8>, usize, usize)>>> = Mutex::default();
}
static LWIP_INIT: Once = Once::new();

pub struct NetStack {
    write: NetStackWrite,
    read: NetStackRead,
}

pub struct NetStackRead {
    receiver: Receiver<(Vec<u8>, usize, usize)>,
    handle: JoinHandle<()>,
}

impl Drop for NetStackRead {
    fn drop(&mut self) {
        let _ = NET_STACK_WRITE.lock().take();
        self.handle.abort_handle();
    }
}

#[derive(Clone)]
pub struct NetStackWrite {}

impl NetStack {
    pub async fn new(head_reserved: usize, tail_reserved: usize, mtu: u16) -> Self {
        let (sender, receiver) = channel::<(Vec<u8>, usize, usize)>(1024);
        {
            HEAD_RESERVED.store(head_reserved);
            TAIL_RESERVED.store(tail_reserved);
            NET_STACK_WRITE.lock().replace(sender);
        }

        let _g = LWIP_MUTEX.lock();
        LWIP_INIT.call_once(|| unsafe {
            lwip_init();
            (*netif_list).output = Some(output_ip4);
        });
        unsafe {
            (*netif_list).mtu = mtu;
        }
        let handle = tokio::spawn(async move {
            loop {
                {
                    let _g = LWIP_MUTEX.lock();
                    unsafe { sys_check_timeouts() };
                }
                tokio::time::sleep(time::Duration::from_millis(250)).await;
            }
        });
        Self {
            write: NetStackWrite {},
            read: NetStackRead { receiver, handle },
        }
    }

    pub async fn recv_ip(&mut self) -> io::Result<(Vec<u8>, usize, usize)> {
        self.read.recv_ip().await
    }
    pub fn send_ip(&self, buf: &[u8]) -> io::Result<()> {
        self.write.send_ip(buf)
    }
    pub fn into_split(self) -> (NetStackWrite, NetStackRead) {
        (self.write, self.read)
    }
}

impl NetStackRead {
    pub async fn recv_ip(&mut self) -> io::Result<(Vec<u8>, usize, usize)> {
        self.receiver
            .recv()
            .await
            .ok_or_else(|| io::Error::new(io::ErrorKind::UnexpectedEof, ""))
    }
}

impl NetStackWrite {
    pub fn send_ip(&self, buf: &[u8]) -> io::Result<()> {
        let _g = LWIP_MUTEX.lock();
        unsafe {
            let pbuf = pbuf_alloc(pbuf_layer_PBUF_RAW, buf.len() as u16_t, pbuf_type_PBUF_RAM);
            pbuf_take(pbuf, buf.as_ptr() as *const raw::c_void, buf.len() as u16_t);
            if let Some(input_fn) = (*netif_list).input {
                let err = input_fn(pbuf, netif_list);
                if err == err_enum_t_ERR_OK as err_t {
                    Ok(())
                } else {
                    pbuf_free(pbuf);
                    Err(io::Error::new(
                        io::ErrorKind::Interrupted,
                        format!("input error: {}", err),
                    ))
                }
            } else {
                pbuf_free(pbuf);
                Err(io::Error::new(
                    io::ErrorKind::Interrupted,
                    "input fn not set",
                ))
            }
        }
    }
}

impl Debug for NetStack {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        unsafe {
            f.debug_struct("NetStack")
                .field("IPv4", &util::to_ip_addr(&(*netif_list).ip_addr))
                .field("mut", &(*netif_list).mtu)
                .finish()
        }
    }
}
