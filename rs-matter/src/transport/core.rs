/*
 *
 *    Copyright (c) 2020-2022 Project CHIP Authors
 *
 *    Licensed under the Apache License, Version 2.0 (the "License");
 *    you may not use this file except in compliance with the License.
 *    You may obtain a copy of the License at
 *
 *        http://www.apache.org/licenses/LICENSE-2.0
 *
 *    Unless required by applicable law or agreed to in writing, software
 *    distributed under the License is distributed on an "AS IS" BASIS,
 *    WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
 *    See the License for the specific language governing permissions and
 *    limitations under the License.
 */

use core::borrow::Borrow;
use core::mem::MaybeUninit;
use core::pin::pin;

use embassy_futures::select::{select, select_slice, Either};
use embassy_sync::{blocking_mutex::raw::NoopRawMutex, channel::Channel};
use embassy_time::{Duration, Timer};

use log::{error, info, warn};

use crate::interaction_model::core::IMStatusCode;
use crate::mdns::Mdns;
use crate::secure_channel::common::SCStatusCodes;
use crate::secure_channel::status_report::{create_status_report, GeneralCode};
use crate::utils::buf::BufferAccess;
use crate::utils::select::Notification;
use crate::{
    alloc,
    data_model::{core::DataModel, objects::DataModelHandler},
    error::{Error, ErrorCode},
    interaction_model::core::PROTO_ID_INTERACTION_MODEL,
    secure_channel::{
        common::{OpCode, PROTO_ID_SECURE_CHANNEL},
        core::SecureChannel,
    },
    transport::packet::Packet,
    utils::select::EitherUnwrap,
    CommissioningData, Matter, MATTER_PORT,
};

use super::{
    exchange::{
        Exchange, ExchangeCtr, ExchangeCtx, ExchangeId, ExchangeState, Role, SessionId,
        MAX_EXCHANGES,
    },
    mrp::ReliableMessage,
    network::{Ipv6Addr, NetworkReceive, NetworkSend, SocketAddr, SocketAddrV6},
    packet::{MAX_RX_BUF_SIZE, MAX_RX_STATUS_BUF_SIZE, MAX_TX_BUF_SIZE},
};

pub const MATTER_SOCKET_BIND_ADDR: SocketAddr =
    SocketAddr::V6(SocketAddrV6::new(Ipv6Addr::UNSPECIFIED, MATTER_PORT, 0, 0));

type TxBuf = MaybeUninit<[u8; MAX_TX_BUF_SIZE]>;
type RxBuf = MaybeUninit<[u8; MAX_RX_BUF_SIZE]>;
type SxBuf = MaybeUninit<[u8; MAX_RX_STATUS_BUF_SIZE]>;

pub struct PacketBuffers {
    tx: [TxBuf; MAX_EXCHANGES],
    rx: [RxBuf; MAX_EXCHANGES],
    sx: [SxBuf; MAX_EXCHANGES + 1],
}

impl PacketBuffers {
    const TX_ELEM: TxBuf = MaybeUninit::uninit();
    const RX_ELEM: RxBuf = MaybeUninit::uninit();
    const SX_ELEM: SxBuf = MaybeUninit::uninit();

    const TX_INIT: [TxBuf; MAX_EXCHANGES] = [Self::TX_ELEM; MAX_EXCHANGES];
    const RX_INIT: [RxBuf; MAX_EXCHANGES] = [Self::RX_ELEM; MAX_EXCHANGES];
    const SX_INIT: [SxBuf; MAX_EXCHANGES + 1] = [Self::SX_ELEM; MAX_EXCHANGES + 1];

    #[inline(always)]
    pub const fn new() -> Self {
        Self {
            tx: Self::TX_INIT,
            rx: Self::RX_INIT,
            sx: Self::SX_INIT,
        }
    }
}

impl<'a> Matter<'a> {
    #[cfg(not(all(
        feature = "std",
        any(target_os = "macos", all(feature = "zeroconf", target_os = "linux"))
    )))]
    pub async fn run_builtin_mdns<S, R>(
        &self,
        send: S,
        recv: R,
        host: crate::mdns::Host<'_>,
        interface: Option<u32>,
    ) -> Result<(), Error>
    where
        S: NetworkSend,
        R: NetworkReceive,
    {
        use crate::mdns::MdnsImpl;

        info!("Running Matter built-in mDNS service");

        if let MdnsImpl::Builtin(mdns) = &self.mdns {
            mdns.run(send, recv, &self.tx_buf, &self.rx_buf, host, interface)
                .await
        } else {
            Err(ErrorCode::MdnsError.into())
        }
    }

    #[allow(clippy::too_many_arguments)]
    pub async fn run<H, S, R>(
        &self,
        send: S,
        recv: R,
        buffers: &mut PacketBuffers,
        dev_comm: CommissioningData,
        handler: &H,
    ) -> Result<(), Error>
    where
        H: DataModelHandler,
        S: NetworkSend,
        R: NetworkReceive,
    {
        info!("Running Matter transport");

        {
            let mut recv_buf = self.rx_buf.get().await;

            if self.start_comissioning(dev_comm, &mut recv_buf)? {
                info!("Comissioning started");
            }
        }

        let construction_notification = Notification::new();

        let mut rx = pin!(self.handle_rx(recv, buffers, &construction_notification, handler));
        let mut tx = pin!(self.handle_tx(send));

        select(&mut rx, &mut tx).await.unwrap()
    }

    #[inline(always)]
    async fn handle_rx<H, R>(
        &self,
        recv: R,
        buffers: &mut PacketBuffers,
        construction_notification: &Notification,
        handler: &H,
    ) -> Result<(), Error>
    where
        H: DataModelHandler,
        R: NetworkReceive,
    {
        info!("Creating queue for {} exchanges", 1);

        let channel = Channel::<NoopRawMutex, _, 1>::new();

        info!("Creating {} handlers", MAX_EXCHANGES);
        let mut handlers = heapless::Vec::<_, MAX_EXCHANGES>::new();

        info!("Handlers size: {}", core::mem::size_of_val(&handlers));

        // Unsafely allow mutable aliasing in the packet pools by different indices
        let pools: *mut PacketBuffers = buffers;

        for index in 0..MAX_EXCHANGES {
            let channel = &channel;
            let handler_id = index;

            let pools = unsafe { pools.as_mut() }.unwrap();

            let tx_buf = unsafe { pools.tx[handler_id].assume_init_mut() };
            let rx_buf = unsafe { pools.rx[handler_id].assume_init_mut() };
            let sx_buf = unsafe { pools.sx[handler_id].assume_init_mut() };

            handlers
                .push(self.exchange_handler(tx_buf, rx_buf, sx_buf, handler_id, channel, handler))
                .map_err(|_| ())
                .unwrap();
        }

        let mut rx = pin!(self.handle_rx_multiplex(
            recv,
            unsafe { buffers.sx[MAX_EXCHANGES].assume_init_mut() },
            construction_notification,
            &channel,
        ));

        let result = select(&mut rx, select_slice(&mut handlers)).await;

        if let Either::First(result) = result {
            if let Err(e) = &result {
                error!("Exitting RX loop due to an error: {:?}", e);
            }

            result?;
        }

        Ok(())
    }

    #[inline(always)]
    pub async fn handle_tx<S>(&self, mut send: S) -> Result<(), Error>
    where
        S: NetworkSend,
    {
        loop {
            loop {
                {
                    let mut send_buf = self.tx_buf.get().await;

                    let mut tx = alloc!(Packet::new_tx(&mut send_buf));

                    if self.pull_tx(&mut tx)? {
                        let addr = tx.peer;

                        let start = tx.get_writebuf()?.get_start();
                        let end = tx.get_writebuf()?.get_tail();

                        send.send_to(&send_buf[start..end], addr).await?;
                    } else {
                        break;
                    }
                }
            }

            self.wait_tx().await?;
        }
    }

    #[inline(always)]
    pub async fn handle_rx_multiplex<'t, 'e, const N: usize, R>(
        &'t self,
        mut receiver: R,
        sts_buf: &mut [u8; MAX_RX_STATUS_BUF_SIZE],
        construction_notification: &'e Notification,
        channel: &Channel<NoopRawMutex, ExchangeCtr<'e>, N>,
    ) -> Result<(), Error>
    where
        R: NetworkReceive,
        't: 'e,
    {
        let mut sts_tx = alloc!(Packet::new_tx(sts_buf));

        loop {
            info!("Transport: waiting for incoming packets");

            receiver.wait_available().await?;

            {
                let mut recv_buf = self.rx_buf.get().await;

                let (len, remote) = receiver.recv_from(&mut recv_buf).await?;

                let mut rx = alloc!(Packet::new_rx(&mut recv_buf[..len]));
                rx.peer = remote;

                if let Some(exchange_ctr) = self
                    .process_rx(construction_notification, &mut rx, &mut sts_tx)
                    .await?
                {
                    let exchange_id = exchange_ctr.id().clone();

                    info!("Transport: got new exchange: {:?}", exchange_id);

                    channel.send(exchange_ctr).await;
                    info!("Transport: exchange sent");

                    self.wait_construction(construction_notification, &rx, &exchange_id)
                        .await?;

                    info!("Transport: exchange started");
                }
            }
        }

        #[allow(unreachable_code)]
        Ok::<_, Error>(())
    }

    #[inline(always)]
    pub async fn exchange_handler<const N: usize, H>(
        &self,
        tx_buf: &mut [u8; MAX_TX_BUF_SIZE],
        rx_buf: &mut [u8; MAX_RX_BUF_SIZE],
        sx_buf: &mut [u8; MAX_RX_STATUS_BUF_SIZE],
        handler_id: impl core::fmt::Display,
        channel: &Channel<NoopRawMutex, ExchangeCtr<'_>, N>,
        handler: &H,
    ) -> Result<(), Error>
    where
        H: DataModelHandler,
    {
        loop {
            let exchange_ctr: ExchangeCtr<'_> = channel.receive().await;

            info!(
                "Handler {}: Got exchange {:?}",
                handler_id,
                exchange_ctr.id()
            );

            let result = self
                .handle_exchange(tx_buf, rx_buf, sx_buf, exchange_ctr, handler)
                .await;

            if let Err(err) = result {
                warn!(
                    "Handler {}: Exchange closed because of error: {:?}",
                    handler_id, err
                );
            } else {
                info!("Handler {}: Exchange completed", handler_id);
            }
        }
    }

    #[inline(always)]
    pub async fn handle_exchange<H>(
        &self,
        tx_buf: &mut [u8; MAX_TX_BUF_SIZE],
        rx_buf: &mut [u8; MAX_RX_BUF_SIZE],
        sx_buf: &mut [u8; MAX_RX_STATUS_BUF_SIZE],
        exchange_ctr: ExchangeCtr<'_>,
        handler: &H,
    ) -> Result<(), Error>
    where
        H: DataModelHandler,
    {
        let mut tx = alloc!(Packet::new_tx(tx_buf.as_mut()));
        let mut rx = alloc!(Packet::new_rx(rx_buf.as_mut()));

        let mut exchange = alloc!(exchange_ctr.get(&mut rx).await?);

        match rx.get_proto_id() {
            PROTO_ID_SECURE_CHANNEL => {
                let sc = SecureChannel::new();

                sc.handle(&mut exchange, &mut rx, &mut tx).await?;

                self.notify_changed();
            }
            PROTO_ID_INTERACTION_MODEL => {
                let dm = DataModel::new(handler);

                let mut rx_status = alloc!(Packet::new_rx(sx_buf));

                dm.handle(&mut exchange, &mut rx, &mut tx, &mut rx_status)
                    .await?;

                self.notify_changed();
            }
            other => {
                error!("Unknown Proto-ID: {}", other);
            }
        }

        Ok(())
    }

    pub fn reset_transport(&self) {
        self.exchanges.borrow_mut().clear();
        self.session_mgr.borrow_mut().reset();
        self.mdns.reset();
    }

    pub async fn process_rx<'r>(
        &'r self,
        construction_notification: &'r Notification,
        src_rx: &mut Packet<'_>,
        sts_tx: &mut Packet<'_>,
    ) -> Result<Option<ExchangeCtr<'r>>, Error> {
        src_rx.plain_hdr_decode()?;

        self.purge()?;

        let (exchange_index, new) = loop {
            let result = self.assign_exchange(&mut self.exchanges.borrow_mut(), src_rx);

            match result {
                Err(e) => match e.code() {
                    ErrorCode::Duplicate => {
                        self.send_notification.signal(());
                        return Ok(None);
                    }
                    // TODO: NoSession, NoExchange and others
                    ErrorCode::NoSpaceSessions => self.evict_session(sts_tx).await?,
                    ErrorCode::NoSpaceExchanges => {
                        self.send_busy(src_rx, sts_tx).await?;
                        return Ok(None);
                    }
                    _ => break Err(e),
                },
                other => break other,
            }
        }?;

        let mut exchanges = self.exchanges.borrow_mut();
        let ctx = &mut exchanges[exchange_index];

        src_rx.log("Got packet");

        if src_rx.proto.is_ack() {
            if new {
                Err(ErrorCode::Invalid)?;
            } else {
                let state = &mut ctx.state;

                match state {
                    ExchangeState::ExchangeRecv {
                        tx_acknowledged, ..
                    } => {
                        *tx_acknowledged = true;
                    }
                    ExchangeState::CompleteAcknowledge { notification, .. } => {
                        unsafe { notification.as_ref() }.unwrap().signal(());
                        ctx.state = ExchangeState::Closed;
                    }
                    _ => {
                        // TODO: Error handling
                        todo!()
                    }
                }

                self.notify_changed();
            }
        }

        if new {
            let constructor = ExchangeCtr {
                exchange: Exchange {
                    id: ctx.id.clone(),
                    matter: self,
                    notification: Notification::new(),
                },
                construction_notification,
            };

            self.notify_changed();

            Ok(Some(constructor))
        } else if src_rx.proto.proto_id == PROTO_ID_SECURE_CHANNEL
            && src_rx.proto.proto_opcode == OpCode::MRPStandAloneAck as u8
        {
            // Standalone ack, do nothing
            Ok(None)
        } else {
            let state = &mut ctx.state;

            match state {
                ExchangeState::ExchangeRecv {
                    rx, notification, ..
                } => {
                    // TODO: Handle Busy status codes

                    let rx = unsafe { rx.as_mut() }.unwrap();
                    rx.load(src_rx)?;

                    unsafe { notification.as_ref() }.unwrap().signal(());
                    *state = ExchangeState::Active;
                }
                _ => {
                    // TODO: Error handling
                    todo!()
                }
            }

            self.notify_changed();

            Ok(None)
        }
    }

    pub async fn wait_construction(
        &self,
        construction_notification: &Notification,
        src_rx: &Packet<'_>,
        exchange_id: &ExchangeId,
    ) -> Result<(), Error> {
        construction_notification.wait().await;

        let mut exchanges = self.exchanges.borrow_mut();

        let ctx = ExchangeCtx::get(&mut exchanges, exchange_id).unwrap();

        let state = &mut ctx.state;

        match state {
            ExchangeState::Construction { rx, notification } => {
                let rx = unsafe { rx.as_mut() }.unwrap();
                rx.load(src_rx)?;

                unsafe { notification.as_ref() }.unwrap().signal(());
                *state = ExchangeState::Active;
            }
            _ => unreachable!(),
        }

        Ok(())
    }

    pub async fn wait_tx(&self) -> Result<(), Error> {
        select(
            self.send_notification.wait(),
            Timer::after(Duration::from_millis(100)),
        )
        .await;

        Ok(())
    }

    pub fn pull_tx(&self, dest_tx: &mut Packet) -> Result<bool, Error> {
        self.purge()?;

        let mut ephemeral = self.ephemeral.borrow_mut();
        let mut exchanges = self.exchanges.borrow_mut();

        self.pull_tx_exchanges(ephemeral.iter_mut().chain(exchanges.iter_mut()), dest_tx)
    }

    fn pull_tx_exchanges<'i, I>(
        &self,
        mut exchanges: I,
        dest_tx: &mut Packet,
    ) -> Result<bool, Error>
    where
        I: Iterator<Item = &'i mut ExchangeCtx>,
    {
        let ctx = exchanges.find(|ctx| {
            matches!(
                &ctx.state,
                ExchangeState::Acknowledge { .. }
                    | ExchangeState::ExchangeSend { .. }
                    // | ExchangeState::ExchangeRecv {
                    //     tx_acknowledged: false,
                    //     ..
                    // }
                    | ExchangeState::Complete { .. } // | ExchangeState::CompleteAcknowledge { .. }
            ) || ctx.mrp.is_ack_ready(*self.borrow())
        });

        if let Some(ctx) = ctx {
            self.notify_changed();

            let state = &mut ctx.state;

            let send = match state {
                ExchangeState::Acknowledge { notification } => {
                    ReliableMessage::prepare_ack(ctx.id.id, dest_tx);

                    unsafe { notification.as_ref() }.unwrap().signal(());
                    *state = ExchangeState::Active;

                    true
                }
                ExchangeState::ExchangeSend {
                    tx,
                    rx,
                    notification,
                } => {
                    let tx = unsafe { tx.as_ref() }.unwrap();
                    dest_tx.load(tx)?;

                    *state = ExchangeState::ExchangeRecv {
                        _tx: tx,
                        tx_acknowledged: false,
                        rx: *rx,
                        notification: *notification,
                    };

                    true
                }
                // ExchangeState::ExchangeRecv { .. } => {
                //     // TODO: Re-send the tx package if due
                //     false
                // }
                ExchangeState::Complete { tx, notification } => {
                    let tx = unsafe { tx.as_ref() }.unwrap();
                    dest_tx.load(tx)?;

                    if dest_tx.is_reliable() {
                        *state = ExchangeState::CompleteAcknowledge {
                            _tx: tx as *const _,
                            notification: *notification,
                        };
                    } else {
                        unsafe { notification.as_ref() }.unwrap().signal(());
                        ctx.state = ExchangeState::Closed;
                    }

                    true
                }
                // ExchangeState::CompleteAcknowledge { .. } => {
                //     // TODO: Re-send the tx package if due
                //     false
                // }
                _ => {
                    ReliableMessage::prepare_ack(ctx.id.id, dest_tx);
                    true
                }
            };

            if send {
                dest_tx.log("Sending packet");
                self.notify_changed();

                return Ok(true);
            }
        }

        Ok(false)
    }

    fn purge(&self) -> Result<(), Error> {
        loop {
            let mut exchanges = self.exchanges.borrow_mut();

            if let Some(index) = exchanges.iter_mut().enumerate().find_map(|(index, ctx)| {
                matches!(ctx.state, ExchangeState::Closed).then_some(index)
            }) {
                exchanges.swap_remove(index);
            } else {
                break;
            }
        }

        Ok(())
    }

    pub(crate) async fn evict_session(&self, tx: &mut Packet<'_>) -> Result<(), Error> {
        let sess_index = self.session_mgr.borrow().get_session_for_eviction();
        if let Some(sess_index) = sess_index {
            let ctx = {
                create_status_report(
                    tx,
                    GeneralCode::Success,
                    PROTO_ID_SECURE_CHANNEL as _,
                    SCStatusCodes::CloseSession as _,
                    None,
                )?;

                let mut session_mgr = self.session_mgr.borrow_mut();
                let session_id = session_mgr.mut_by_index(sess_index).unwrap().id();
                warn!("Evicting session: {:?}", session_id);

                let ctx = ExchangeCtx::prep_ephemeral(session_id, &mut session_mgr, None, tx)?;

                session_mgr.remove(sess_index);

                ctx
            };

            self.send_ephemeral(ctx, tx).await
        } else {
            Err(ErrorCode::NoSpaceSessions.into())
        }
    }

    async fn send_busy(&self, rx: &Packet<'_>, tx: &mut Packet<'_>) -> Result<(), Error> {
        warn!("Sending Busy as all exchanges are occupied");

        create_status_report(
            tx,
            GeneralCode::Busy,
            rx.get_proto_id() as _,
            if rx.get_proto_id() == PROTO_ID_SECURE_CHANNEL {
                SCStatusCodes::Busy as _
            } else {
                IMStatusCode::Busy as _
            },
            None, // TODO: ms
        )?;

        let ctx = ExchangeCtx::prep_ephemeral(
            SessionId::load(rx),
            &mut self.session_mgr.borrow_mut(),
            Some(rx),
            tx,
        )?;

        self.send_ephemeral(ctx, tx).await
    }

    async fn send_ephemeral(&self, mut ctx: ExchangeCtx, tx: &mut Packet<'_>) -> Result<(), Error> {
        let _guard = self.ephemeral_mutex.lock().await;

        let notification = Notification::new();

        let tx: &'static mut Packet<'static> = unsafe { core::mem::transmute(tx) };

        ctx.state = ExchangeState::Complete {
            tx,
            notification: &notification,
        };

        *self.ephemeral.borrow_mut() = Some(ctx);

        self.send_notification.signal(());

        notification.wait().await;

        *self.ephemeral.borrow_mut() = None;

        Ok(())
    }

    fn assign_exchange(
        &self,
        exchanges: &mut heapless::Vec<ExchangeCtx, MAX_EXCHANGES>,
        rx: &mut Packet<'_>,
    ) -> Result<(usize, bool), Error> {
        // Get the session

        let mut session_mgr = self.session_mgr.borrow_mut();

        let sess_index = session_mgr.post_recv(rx)?;
        let session = session_mgr.mut_by_index(sess_index).unwrap();

        // Decrypt the message
        session.recv(self.epoch, rx)?;

        // Get the exchange
        let (exchange_index, new) = Self::register(
            exchanges,
            ExchangeId::load(rx),
            Role::complementary(rx.proto.is_initiator()),
            // We create a new exchange, only if the peer is the initiator
            rx.proto.is_initiator(),
        )?;

        // Message Reliability Protocol
        exchanges[exchange_index].mrp.recv(rx, self.epoch)?;

        Ok((exchange_index, new))
    }

    fn register(
        exchanges: &mut heapless::Vec<ExchangeCtx, MAX_EXCHANGES>,
        id: ExchangeId,
        role: Role,
        create_new: bool,
    ) -> Result<(usize, bool), Error> {
        let exchange_index = exchanges
            .iter_mut()
            .enumerate()
            .find_map(|(index, exchange)| (exchange.id == id).then_some(index));

        if let Some(exchange_index) = exchange_index {
            let exchange = &mut exchanges[exchange_index];
            if exchange.role == role {
                Ok((exchange_index, false))
            } else {
                Err(ErrorCode::NoExchange.into())
            }
        } else if create_new {
            info!("Creating new exchange: {:?}", id);

            let exchange = ExchangeCtx {
                id,
                role,
                mrp: ReliableMessage::new(),
                state: ExchangeState::Active,
            };

            exchanges
                .push(exchange)
                .map_err(|_| ErrorCode::NoSpaceExchanges)?;

            Ok((exchanges.len() - 1, true))
        } else {
            Err(ErrorCode::NoExchange.into())
        }
    }
}
