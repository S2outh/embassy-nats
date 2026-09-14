use embedded_io_async::Read;
use thiserror::Error;

use crate::{BytesBuf, StrBuf, CapacityError, DELIM, NatsCollections, NatsInfoMsg, NatsMsg};

macro_rules! defmt {
    ($($t:tt)*) => {{
        #[cfg(feature = "defmt")]
        defmt::$($t)*;
    }};
}

#[cfg_attr(feature = "defmt", derive(defmt::Format))]
#[derive(Debug, Error)]
pub enum FramerError<R> {
    #[error("Disconnected")]
    Disconnected,
    #[error("Read error: {0}")]
    Read(R),
    #[error("Capacity of collection {0} not sufficient for operation")]
    Capacity(#[from] CapacityError),
    #[error("Json deserialization error {0}")]
    Deser(#[from] serde_json_core::de::Error),
    #[error("Invalid utf8")]
    Utf8,
    #[error("Invalid header")]
    Header,
}

impl<R> From<core::str::Utf8Error> for FramerError<R> {
    fn from(_: core::str::Utf8Error) -> Self {
        FramerError::Utf8
    }
}

/// A Received Nats protocol frame
pub enum Frame<C: NatsCollections> {
    Ping,
    Info(NatsInfoMsg),
    Err,
    Ok,
    Msg(NatsMsg<C>),
}

/// A result enum from an internal frame
enum InternalFrame<C: NatsCollections> {
    MsgHeader {
        topic: C::Topic,
        len: usize,
        sid: usize,
    },
    MsgDone,
    Ping,
    Info(NatsInfoMsg),
    Err,
    Ok,
    None,
}

/// This framer type is used to sync and read frames from a provided "read" function.
/// This read function is expected to follow the embedded_io_async contract of always
/// reading at least one, but up to buf.len() bytes. Since the embassy_runner also needs to perform
/// other operations besides reading from the wire this framer is itself non blocking:
/// the frame() function will call read() once, and return Option::None or Option::Some(frame)
/// depending on whether or not the read bytes are sufficient to complete a frame.
/// Conversely calling frame() does not guarantee that all of the currently ready bytes are beeing
/// read.
pub enum Framer<C: NatsCollections> {
    Sync(SyncFramer<C>),
    Msg(MsgFramer<C>),
}

pub struct SyncFramer<C: NatsCollections> {
    buffer: C::SyncBuf,
    magic_pos: usize,
}

pub struct MsgFramer<C: NatsCollections> {
    buffer: C::MsgBuf,
    topic: C::Topic,
    pos: usize,
    len: usize,
    sid: usize,
}

impl<C: NatsCollections> Framer<C> {
    /// create a new framer in sync state
    pub fn new() -> Self {
        Self::Sync(SyncFramer::new())
    }
    /// Call the provided reader.read() function exactly once,
    /// and return a frame if it can be finished
    pub async fn frame<R: Read>(&mut self, reader: &mut R) -> Result<Option<Frame<C>>, FramerError<R::Error>> {
        let internal_frame = match self {
            Self::Sync(framer) => framer.frame(reader).await,
            Self::Msg(framer) => framer.frame(reader).await,
        }?;
        match internal_frame {
            InternalFrame::MsgHeader { topic, len, sid } => {
                *self = Self::Msg(MsgFramer::new(topic, len, sid));
                Ok(None)
            },
            InternalFrame::MsgDone => {
                let Self::Msg(framer) = core::mem::replace(self, Self::Sync(SyncFramer::new())) else { panic!() };
                Ok(Some(framer.finalize()))
            },
            InternalFrame::Ok => Ok(Some(Frame::Ok)),
            InternalFrame::Err => Ok(Some(Frame::Err)),
            InternalFrame::Ping => Ok(Some(Frame::Ping)),
            InternalFrame::Info(info) => Ok(Some(Frame::Info(info))),
            InternalFrame::None => Ok(None),
        }
    }


}

impl<C: NatsCollections> SyncFramer<C> {
    fn new() -> Self {
        Self {
            buffer: C::SyncBuf::default(),
            magic_pos: 0,
        }
    }

    async fn frame<R: Read>(&mut self, reader: &mut R) -> Result<InternalFrame<C>, FramerError<R::Error>> {
        let slice = self.buffer.extend_by(1)?;
        let n = reader.read(slice).await.map_err(|e| FramerError::Read(e))?;
        if n == 0 {
           return Err(FramerError::Disconnected);
        }

        if slice[0] == DELIM[self.magic_pos] {
            self.magic_pos += 1;
            if self.magic_pos == DELIM.len() {
                self.magic_pos = 0;
                let header = self.parse_header();
                self.buffer.clear();
                header
            } else {
                Ok(InternalFrame::None)
            }
        } else {
            self.magic_pos = 0;
            Ok(InternalFrame::None)
        }
    }

    fn parse_header<E>(&mut self) -> Result<InternalFrame<C>, FramerError<E>> {
        fn parse_msg_header<E, C: NatsCollections>(msg: &str) -> Result<InternalFrame<C>, FramerError<E>> {
            let Some((topic, msg)) = msg.split_once(' ') else {
                defmt!(error!("nats msg header parsing error (1)"));
                return Err(FramerError::Header);
            };
            let Some((sid, msg)) = msg.split_once(' ') else {
                defmt!(error!("nats msg header parsing error (2)"));
                return Err(FramerError::Header);
            };
            let (_reply_to, len) = msg.split_once(' ').unwrap_or(("", msg));
            let Ok(sid) = sid.parse::<usize>() else {
                defmt!(error!("nats sid parsing error: '{}'", sid));
                return Err(FramerError::Header);
            };
            let Ok(len) = len.parse::<usize>() else {
                defmt!(error!("nats msg len parsing error: '{}'", len));
                return Err(FramerError::Header);
            };

            Ok(InternalFrame::MsgHeader {
                len,
                sid,
                topic: C::Topic::try_from_str(topic)?
            })
        }

        let mut packet_str = core::str::from_utf8(&self.buffer.as_bytes())?;
        packet_str = &packet_str[..(packet_str.len() - 2)];

        if packet_str.is_empty() {
            return Ok(InternalFrame::None)
        }

        let (cmd, msg) = packet_str
            .trim()
            .split_once(' ')
            .unwrap_or((&packet_str.trim(), ""));

        // match first word to one of the Nats messages supported by this
        // implementations
        match cmd {
            "PING" => Ok(InternalFrame::Ping),
            "INFO" => {
                match serde_json_core::from_str::<NatsInfoMsg>(msg) {
                    Ok((info, _)) => Ok(InternalFrame::Info(info)),
                    Err(e) => {
                        defmt!(warn!("could not decode nats INFO"));
                        Err(FramerError::Deser(e))
                    }
                }
            }
            "-ERR" => {
                defmt!(error!("nats disconnected ({})", msg));
                Ok(InternalFrame::Err)
            }
            "+OK" => Ok(InternalFrame::Ok),
            "MSG" => parse_msg_header(msg),
            _default => {
                defmt!(warn!("unknown nats cmd {}", _default));
                Err(FramerError::Header)
            }
        }
    }
}

impl <C: NatsCollections> MsgFramer<C> {
    fn new(topic: C::Topic, len: usize, sid: usize) -> Self {
        Self {
            buffer: C::MsgBuf::default(),
            pos: 0,
            topic,
            len,
            sid,
        }
    }
    async fn frame<R: Read>(&mut self, reader: &mut R) -> Result<InternalFrame<C>, FramerError<R::Error>> {
        let slice = self.buffer.extend_by(self.len - self.pos)?;
        let n = reader.read(slice).await.map_err(|e| FramerError::Read(e))?;
        if n == 0 && self.len > 0 {
           return Err(FramerError::Disconnected);
        }
        
        self.pos += n;
        self.buffer.truncate(self.pos);
        if self.pos >= self.len {
            Ok(InternalFrame::MsgDone)
        } else {
            Ok(InternalFrame::None)
        }
    }
    fn finalize(self) -> Frame<C> {
        Frame::Msg(NatsMsg { sid: self.sid, topic: self.topic, data: self.buffer })
    }
}
