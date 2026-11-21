mod responder_stream;
#[cfg(test)]
mod responder_test;

use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use async_trait::async_trait;
use responder_stream::ResponderStream;
use rtcp::transport_feedbacks::transport_layer_nack::TransportLayerNack;
use tokio::sync::Mutex;

use crate::error::Result;
use crate::nack::stream_support_nack;
use crate::stream_info::StreamInfo;
use crate::{
    Attributes, Interceptor, InterceptorBuilder, RTCPReader, RTCPWriter, RTPReader, RTPWriter,
};

/// GeneratorBuilder can be used to configure Responder Interceptor
#[derive(Default)]
pub struct ResponderBuilder {
    log2_size: Option<u8>,
}

impl ResponderBuilder {
    /// with_log2_size sets the size of the interceptor.
    /// Size must be one of: 1, 2, 4, 8, 16, 32, 64, 128, 256, 512, 1024, 2048, 4096, 8192, 16384, 32768
    pub fn with_log2_size(mut self, log2_size: u8) -> ResponderBuilder {
        self.log2_size = Some(log2_size);
        self
    }
}

impl InterceptorBuilder for ResponderBuilder {
    fn build(&self, _id: &str) -> Result<Arc<dyn Interceptor + Send + Sync>> {
        Ok(Arc::new(Responder {
            internal: Arc::new(ResponderInternal {
                log2_size: self.log2_size.unwrap_or(13), // 8192 = 1 << 13
                streams: Arc::new(Mutex::new(HashMap::new())),
                rtx: Arc::new(Mutex::new(HashMap::new())),
            }),
        }))
    }
}

/// RTX 重传目标：主流 SSRC -> (RTX writer, RTX SSRC, RTX payload type, RTX 独立序列)
/// RFC 4588 要求重传包必须走 RTX 流（独立 SSRC 与序列号），否则浏览器当重复包丢弃。
#[derive(Clone)]
struct RtxBinding {
    writer: Arc<dyn RTPWriter + Send + Sync>,
    ssrc: u32,
    payload_type: u8,
    seq: Arc<std::sync::atomic::AtomicU16>,
}

pub struct ResponderInternal {
    log2_size: u8,
    streams: Arc<Mutex<HashMap<u32, Arc<ResponderStream>>>>,
    rtx: Arc<Mutex<HashMap<u32, RtxBinding>>>,
}

impl ResponderInternal {
    async fn resend_packets(
        streams: Arc<Mutex<HashMap<u32, Arc<ResponderStream>>>>,
        rtx_map: Arc<Mutex<HashMap<u32, RtxBinding>>>,
        nack: TransportLayerNack,
    ) {
        // 该主流若协商了 RTX，重传必须改写成 RTX 包，否则浏览器按重复包丢弃
        let rtx = { rtx_map.lock().await.get(&nack.media_ssrc).cloned() };
        let lookup = { streams.lock().await.get(&nack.media_ssrc).cloned() };
        let stream = match lookup {
            Some(s) => s,
            None => return,
        };

        for n in &nack.nacks {
            // can't use n.range() since this callback is async fn,
            // instead, use NackPair into_iter()
            let stream2 = Arc::clone(&stream);
            let rtx2 = rtx.clone();
            let f = Box::new(
                move |seq: u16| -> Pin<Box<dyn Future<Output = bool> + Send + 'static>> {
                    let stream3 = Arc::clone(&stream2);
                    let rtx3 = rtx2.clone();
                    Box::pin(async move {
                        if let Some(p) = stream3.get(seq).await {
                            let a = Attributes::new();
                            match &rtx3 {
                                Some(r) => {
                                    // RFC 4588：改写成 RTX 包——
                                    // SSRC/PT 换成 RTX 的、序列号用 RTX 独立序列、
                                    // payload 前置 2 字节原始序列号。
                                    let rtx_seq = r
                                        .seq
                                        .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                                    let mut pl = Vec::with_capacity(2 + p.payload.len());
                                    pl.extend_from_slice(&p.header.sequence_number.to_be_bytes());
                                    pl.extend_from_slice(&p.payload);
                                    let mut h = p.header.clone();
                                    h.ssrc = r.ssrc;
                                    h.payload_type = r.payload_type;
                                    h.sequence_number = rtx_seq;
                                    h.extension = false;
                                    h.extension_profile = 0;
                                    h.extensions.clear();
                                    h.extensions_padding = 0;
                                    h.csrc.clear();
                                    let np = rtp::packet::Packet {
                                        header: h,
                                        payload: bytes::Bytes::from(pl),
                                    };
                                    if let Err(e) = r.writer.write(&np, &a).await {
                                        log::warn!("rtx resend failed: {}", e);
                                    }
                                }
                                None => {
                                    // 没有 RTX（对端未协商）：退回原 SSRC 重发
                                    if let Err(err) = stream3.next_rtp_writer.write(&p, &a).await
                                    {
                                        log::warn!("failed resending nacked packet: {}", err);
                                    }
                                }
                            }
                        }
                        true
                    })
                },
            );
            for packet_id in n.into_iter() {
                if !f(packet_id).await {
                    return;
                }
            }
        }
    }
}

pub struct ResponderRtcpReader {
    parent_rtcp_reader: Arc<dyn RTCPReader + Send + Sync>,
    internal: Arc<ResponderInternal>,
}

#[async_trait]
impl RTCPReader for ResponderRtcpReader {
    async fn read(
        &self,
        buf: &mut [u8],
        a: &Attributes,
    ) -> Result<(Vec<Box<dyn rtcp::packet::Packet + Send + Sync>>, Attributes)> {
        let (pkts, attr) = { self.parent_rtcp_reader.read(buf, a).await? };
        for p in &pkts {
            if let Some(nack) = p.as_any().downcast_ref::<TransportLayerNack>() {
                let nack = nack.clone();
                let streams = Arc::clone(&self.internal.streams);
                let rtx = Arc::clone(&self.internal.rtx);
                tokio::spawn(async move {
                    ResponderInternal::resend_packets(streams, rtx, nack).await;
                });
            }
        }

        Ok((pkts, attr))
    }
}

/// Responder responds to nack feedback messages
pub struct Responder {
    internal: Arc<ResponderInternal>,
}

impl Responder {
    /// builder returns a new ResponderBuilder.
    pub fn builder() -> ResponderBuilder {
        ResponderBuilder::default()
    }
}

#[async_trait]
impl Interceptor for Responder {
    /// bind_rtcp_reader lets you modify any incoming RTCP packets. It is called once per sender/receiver, however this might
    /// change in the future. The returned method will be called once per packet batch.
    async fn bind_rtcp_reader(
        &self,
        reader: Arc<dyn RTCPReader + Send + Sync>,
    ) -> Arc<dyn RTCPReader + Send + Sync> {
        Arc::new(ResponderRtcpReader {
            internal: Arc::clone(&self.internal),
            parent_rtcp_reader: reader,
        }) as Arc<dyn RTCPReader + Send + Sync>
    }

    /// bind_rtcp_writer lets you modify any outgoing RTCP packets. It is called once per PeerConnection. The returned method
    /// will be called once per packet batch.
    async fn bind_rtcp_writer(
        &self,
        writer: Arc<dyn RTCPWriter + Send + Sync>,
    ) -> Arc<dyn RTCPWriter + Send + Sync> {
        writer
    }

    /// bind_local_stream lets you modify any outgoing RTP packets. It is called once for per LocalStream. The returned method
    /// will be called once per rtp packet.
    async fn bind_local_stream(
        &self,
        info: &StreamInfo,
        writer: Arc<dyn RTPWriter + Send + Sync>,
    ) -> Arc<dyn RTPWriter + Send + Sync> {
        // RTX 流：associated_stream 指向主流。记下 RTX 的 writer/SSRC/PT，
        // 供主流收到 NACK 时把包改写成 RTX 格式重发（RFC 4588）。
        if let Some(assoc) = &info.associated_stream {
            let mut m = self.internal.rtx.lock().await;
            m.insert(
                assoc.ssrc,
                RtxBinding {
                    writer: writer.clone(),
                    ssrc: info.ssrc,
                    payload_type: info.payload_type,
                    seq: Arc::new(std::sync::atomic::AtomicU16::new(0)),
                },
            );
            return writer;
        }
        if !stream_support_nack(info) {
            return writer;
        }

        let stream = Arc::new(ResponderStream::new(self.internal.log2_size, writer));
        {
            let mut streams = self.internal.streams.lock().await;
            streams.insert(info.ssrc, Arc::clone(&stream));
        }

        stream
    }

    /// unbind_local_stream is called when the Stream is removed. It can be used to clean up any data related to that track.
    async fn unbind_local_stream(&self, info: &StreamInfo) {
        let mut streams = self.internal.streams.lock().await;
        streams.remove(&info.ssrc);
    }

    /// bind_remote_stream lets you modify any incoming RTP packets. It is called once for per RemoteStream. The returned method
    /// will be called once per rtp packet.
    async fn bind_remote_stream(
        &self,
        _info: &StreamInfo,
        reader: Arc<dyn RTPReader + Send + Sync>,
    ) -> Arc<dyn RTPReader + Send + Sync> {
        reader
    }

    /// unbind_remote_stream is called when the Stream is removed. It can be used to clean up any data related to that track.
    async fn unbind_remote_stream(&self, _info: &StreamInfo) {}

    /// close closes the Interceptor, cleaning up any data if necessary.
    async fn close(&self) -> Result<()> {
        Ok(())
    }
}
