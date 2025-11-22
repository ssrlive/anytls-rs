use crate::proxy::padding::PaddingFactory;
use crate::proxy::session::frame::{
    Frame, CMD_ALERT, CMD_FIN, CMD_HEART_REQUEST, CMD_HEART_RESPONSE, CMD_PSH, CMD_SERVER_SETTINGS,
    CMD_SETTINGS, CMD_SYN, CMD_SYNACK, CMD_UPDATE_PADDING_SCHEME,
};
use crate::proxy::session::Stream;
use crate::util::{
    string_map::{StringMap, StringMapExt},
    PROGRAM_VERSION_NAME,
};
use std::collections::HashMap;
use std::io;
use std::sync::Arc;
use tokio::sync::{Mutex, RwLock};

pub struct Session {
    streams: Arc<Mutex<HashMap<u32, Arc<Stream>>>>,
    stream_id: Arc<Mutex<u32>>,
    closed: Arc<Mutex<bool>>,
    is_client: bool,
    peer_version: Arc<Mutex<u8>>,
    padding: Arc<RwLock<PaddingFactory>>,
    send_padding: Arc<Mutex<bool>>,
    buffering: Arc<Mutex<bool>>,
    buffer: Arc<Mutex<Vec<u8>>>,
    pkt_counter: Arc<Mutex<u32>>,
    idle_notify: Arc<tokio::sync::Notify>,
}

impl Session {
    pub fn new_client(
        _conn: Box<dyn crate::util::r#type::AsyncReadWrite>,
        padding: Arc<RwLock<PaddingFactory>>,
    ) -> Self {
        Self {
            streams: Arc::new(Mutex::new(HashMap::new())),
            stream_id: Arc::new(Mutex::new(0)),
            closed: Arc::new(Mutex::new(false)),
            is_client: true,
            peer_version: Arc::new(Mutex::new(0)),
            padding,
            send_padding: Arc::new(Mutex::new(true)),
            buffering: Arc::new(Mutex::new(false)),
            buffer: Arc::new(Mutex::new(Vec::new())),
            pkt_counter: Arc::new(Mutex::new(0)),
            idle_notify: Arc::new(tokio::sync::Notify::new()),
        }
    }
    
    pub fn new_server(
        _conn: Box<dyn crate::util::r#type::AsyncReadWrite>,
        _on_new_stream: Box<dyn Fn(Arc<Stream>) + Send + Sync>,
        padding: Arc<RwLock<PaddingFactory>>,
    ) -> Self {
        Self {
            streams: Arc::new(Mutex::new(HashMap::new())),
            stream_id: Arc::new(Mutex::new(0)),
            closed: Arc::new(Mutex::new(false)),
            is_client: false,
            peer_version: Arc::new(Mutex::new(0)),
            padding,
            send_padding: Arc::new(Mutex::new(false)),
            buffering: Arc::new(Mutex::new(false)),
            buffer: Arc::new(Mutex::new(Vec::new())),
            pkt_counter: Arc::new(Mutex::new(0)),
            idle_notify: Arc::new(tokio::sync::Notify::new()),
        }
    }
    
    pub async fn run(&self) -> io::Result<()> {
        if self.is_client {
            self.send_settings().await?;
        }
        
        self.recv_loop().await
    }
    
    async fn send_settings(&self) -> io::Result<()> {
        let mut settings = StringMap::new();
        settings.insert("v".to_string(), "2".to_string());
        settings.insert("client".to_string(), PROGRAM_VERSION_NAME.to_string());
        
        let padding = self.padding.read().await;
        settings.insert("padding-md5".to_string(), padding.md5().to_string());
        drop(padding);
        
        let frame = Frame::with_data(CMD_SETTINGS, 0, settings.to_bytes().into());
        self.write_frame(frame).await?;
        
        let mut buffering = self.buffering.lock().await;
        *buffering = true;
        
        Ok(())
    }
    
    async fn recv_loop(&self) -> io::Result<()> {
        loop {
            if *self.closed.lock().await {
                return Err(io::Error::new(io::ErrorKind::BrokenPipe, "Session closed"));
            }
            
            // Simplified implementation - in a real implementation, this would read from the connection
            tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;
        }
    }
    
    async fn _handle_frame(&self, cmd: u8, sid: u32, data: Vec<u8>) -> io::Result<()> {
        match cmd {
            CMD_PSH => {
                if !data.is_empty() {
                    let streams = self.streams.lock().await;
                    if let Some(_stream) = streams.get(&sid) {
                        // Write data to stream
                        // This would need proper implementation
                    }
                }
            }
            CMD_SYN => {
                if !self.is_client {
                    let mut streams = self.streams.lock().await;
                    if !streams.contains_key(&sid) {
                        let stream = Arc::new(Stream::new(sid));
                        streams.insert(sid, stream);
                    }
                }
            }
            CMD_FIN => {
                let mut streams = self.streams.lock().await;
                if let Some(stream) = streams.remove(&sid) {
                    stream.close().await?;
                }
                if streams.is_empty() {
                    self.idle_notify.notify_waiters();
                }
            }
            CMD_SETTINGS => {
                if !self.is_client && !data.is_empty() {
                    let _settings = StringMap::from_bytes(&data);
                    // Handle settings
                }
            }
            CMD_ALERT => {
                if !data.is_empty() {
                    let message = String::from_utf8_lossy(&data);
                    log::error!("Alert from server: {}", message);
                }
                return Err(io::Error::new(io::ErrorKind::Other, "Alert received"));
            }
            CMD_UPDATE_PADDING_SCHEME => {
                if !data.is_empty() && self.is_client {
                    // Update padding scheme
                }
            }
            CMD_HEART_REQUEST => {
                let frame = Frame::new(CMD_HEART_RESPONSE, sid);
                self.write_frame(frame).await?;
            }
            CMD_HEART_RESPONSE => {
                // Handle heartbeat response
            }
            CMD_SERVER_SETTINGS => {
                if !data.is_empty() && self.is_client {
                    let _settings = StringMap::from_bytes(&data);
                    // Handle server settings
                }
            }
            CMD_SYNACK => {
                // Handle SYNACK
            }
            _ => {
                // Unknown command
            }
        }
        Ok(())
    }
    
    async fn _read_exact(&self, n: usize) -> io::Result<Vec<u8>> {
        let buffer = vec![0u8; n];
        Ok(buffer)
    }
    
    pub async fn write_frame(&self, frame: Frame) -> io::Result<usize> {
        let data = frame.to_bytes();
        self.write_conn(&data).await
    }
    
    async fn write_conn(&self, data: &[u8]) -> io::Result<usize> {
        // Simplified implementation
        // In a real implementation, this would handle padding and buffering
        Ok(data.len())
    }
    
    pub async fn open_stream(&self) -> io::Result<Arc<Stream>> {
        let mut stream_id = self.stream_id.lock().await;
        *stream_id += 1;
        let id = *stream_id;
        drop(stream_id);
        
        let stream = Arc::new(Stream::new(id));
        
        let frame = Frame::new(CMD_SYN, id);
        self.write_frame(frame).await?;
        
        let mut streams = self.streams.lock().await;
        streams.insert(id, stream.clone());
        
        Ok(stream)
    }
    
    pub async fn stream_closed(&self, sid: u32) -> io::Result<()> {
        let frame = Frame::new(CMD_FIN, sid);
        self.write_frame(frame).await?;
        
        let mut streams = self.streams.lock().await;
        streams.remove(&sid);
        if streams.is_empty() {
            self.idle_notify.notify_waiters();
        }
        
        Ok(())
    }
    
    pub async fn close(&self) -> io::Result<()> {
        let mut closed = self.closed.lock().await;
        if *closed {
            return Ok(());
        }
        *closed = true;
        drop(closed);
        
        let streams = self.streams.lock().await;
        for stream in streams.values() {
            let _ = stream.close().await;
        }
        
        Ok(())
    }
    
    pub async fn is_closed(&self) -> bool {
        *self.closed.lock().await
    }
    
    pub async fn peer_version(&self) -> u8 {
        *self.peer_version.lock().await
    }

    pub async fn wait_for_idle(&self) {
        self.idle_notify.notified().await;
    }
}

impl Clone for Session {
    fn clone(&self) -> Self {
        Self {
            streams: self.streams.clone(),
            stream_id: self.stream_id.clone(),
            closed: self.closed.clone(),
            is_client: self.is_client,
            peer_version: self.peer_version.clone(),
            padding: self.padding.clone(),
            send_padding: self.send_padding.clone(),
            buffering: self.buffering.clone(),
            buffer: self.buffer.clone(),
            pkt_counter: self.pkt_counter.clone(),
            idle_notify: self.idle_notify.clone(),
        }
    }
}