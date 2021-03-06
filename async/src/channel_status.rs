use either::Either;
use parking_lot::RwLock;

use std::sync::Arc;

use crate::requests::RequestId;

#[derive(Clone, Debug, Default)]
pub struct ChannelStatus {
  inner: Arc<RwLock<Inner>>,
}

impl ChannelStatus {
  pub fn is_initializing(&self) -> bool {
    self.inner.read().state == ChannelState::Initial
  }

  pub fn is_connected(&self) -> bool {
    !&[ChannelState::Initial, ChannelState::Closing, ChannelState::Closed, ChannelState::Error].contains(&self.inner.read().state)
  }

  pub fn confirm(&self) -> bool {
    self.inner.read().confirm
  }

  pub fn set_confirm(&self) {
    self.inner.write().confirm = true
  }

  pub fn state(&self) -> ChannelState {
    self.inner.read().state.clone()
  }

  pub fn set_state(&self, state: ChannelState) {
    self.inner.write().state = state
  }

  pub fn set_send_flow(&self, flow: bool) {
    self.inner.write().send_flow = flow;
  }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ChannelState {
    Initial,
    Connected,
    Closing,
    Closed,
    Error,
    SendingContent(usize),
    WillReceiveContent(Option<String>, Either<RequestId, String>),
    ReceivingContent(Option<String>, Either<RequestId, String>, usize),
}

impl Default for ChannelState {
  fn default() -> Self {
    ChannelState::Initial
  }
}

#[derive(Debug)]
struct Inner {
  confirm:   bool,
  send_flow: bool, // FIXME: we should respect that
  state:     ChannelState,
}

impl Default for Inner {
  fn default() -> Self {
    Self {
      confirm:   false,
      send_flow: true,
      state:     ChannelState::default(),
    }
  }
}
