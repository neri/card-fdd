//! Allows CBI to choose the ADSC status stage before starting a command's data phase.

use core::sync::atomic::{AtomicU32, Ordering::Relaxed};

use embassy_futures::select::{Either, select};
use embassy_sync::{blocking_mutex::raw::CriticalSectionRawMutex, signal::Signal};
use embassy_time::{Duration, with_timeout};
use embassy_usb::driver::{
    ControlPipe, Driver, EndpointAddress, EndpointAllocError, EndpointError, EndpointType,
};

static NEXT: AtomicU32 = AtomicU32::new(0);
static ACTIVE: AtomicU32 = AtomicU32::new(0);
static STALL: Signal<CriticalSectionRawMutex, bool> = Signal::new();

/// The USB and MSC tasks share one executor. Arming, replying and cancelling do not yield.
pub(super) fn defer() -> u32 {
    let token = NEXT.load(Relaxed).wrapping_add(1).max(1);
    NEXT.store(token, Relaxed);
    STALL.reset();
    ACTIVE.store(token, Relaxed);
    token
}

pub(super) fn cancel() {
    ACTIVE.store(0, Relaxed);
    STALL.reset();
}

pub(super) fn reply(token: u32, stall: bool) -> bool {
    if ACTIVE.load(Relaxed) != token {
        return false;
    }
    STALL.signal(stall);
    true
}

pub struct StatusDriver<D>(pub D);

impl<'d, D: Driver<'d>> Driver<'d> for StatusDriver<D> {
    type EndpointOut = D::EndpointOut;
    type EndpointIn = D::EndpointIn;
    type ControlPipe = StatusPipe<D::ControlPipe>;
    type Bus = D::Bus;

    fn alloc_endpoint_out(
        &mut self,
        ep_type: EndpointType,
        addr: Option<EndpointAddress>,
        size: u16,
        interval: u8,
    ) -> Result<Self::EndpointOut, EndpointAllocError> {
        self.0.alloc_endpoint_out(ep_type, addr, size, interval)
    }

    fn alloc_endpoint_in(
        &mut self,
        ep_type: EndpointType,
        addr: Option<EndpointAddress>,
        size: u16,
        interval: u8,
    ) -> Result<Self::EndpointIn, EndpointAllocError> {
        self.0.alloc_endpoint_in(ep_type, addr, size, interval)
    }

    fn start(self, max_packet_size: u16) -> (Self::Bus, Self::ControlPipe) {
        let (bus, pipe) = self.0.start(max_packet_size);
        (
            bus,
            StatusPipe {
                pipe,
                next_setup: None,
            },
        )
    }
}

pub struct StatusPipe<C> {
    pipe: C,
    next_setup: Option<([u8; 8], u32)>,
}

impl<C: ControlPipe> ControlPipe for StatusPipe<C> {
    fn max_packet_size(&self) -> usize {
        self.pipe.max_packet_size()
    }

    async fn setup(&mut self) -> [u8; 8] {
        match self.next_setup.take() {
            Some((setup, resets)) if resets == super::HOST.stats.bus_resets.load(Relaxed) => setup,
            _ => self.pipe.setup().await,
        }
    }

    async fn data_out(
        &mut self,
        buf: &mut [u8],
        first: bool,
        last: bool,
    ) -> Result<usize, EndpointError> {
        self.pipe.data_out(buf, first, last).await
    }

    async fn data_in(&mut self, data: &[u8], first: bool, last: bool) -> Result<(), EndpointError> {
        self.pipe.data_in(data, first, last).await
    }

    async fn accept(&mut self) {
        if ACTIVE.load(Relaxed) == 0 {
            self.pipe.accept().await;
            return;
        }

        // A host can abandon the current request at any time. Preserve the replacement SETUP.
        // Bound the wait in case a previous MSC data phase is still blocked on the host.
        let status = select(
            self.pipe.setup(),
            with_timeout(Duration::from_millis(100), STALL.wait()),
        )
        .await;
        cancel();
        match status {
            Either::First(setup) => {
                self.next_setup = Some((setup, super::HOST.stats.bus_resets.load(Relaxed)));
            }
            Either::Second(Ok(false)) => self.pipe.accept().await,
            Either::Second(Ok(true)) => {
                super::command::UsbStats::inc(&super::HOST.stats.adsc_stalls);
                self.pipe.reject().await;
            }
            Either::Second(Err(_)) => {
                log::warn!("CBI: timed out waiting for command status before the ADSC status stage");
                self.pipe.reject().await;
            }
        }
    }

    async fn reject(&mut self) {
        self.pipe.reject().await;
    }

    async fn accept_set_address(&mut self, address: u8) {
        self.pipe.accept_set_address(address).await;
    }
}
