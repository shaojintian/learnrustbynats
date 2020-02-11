use crate::error::*;
use crate::parser::{ParseResult, Parser, PubArg, SubArg};
use crate::server::*;
use crate::simple_sublist::{ArcSubResult, ArcSubscription, SubListTrait, Subscription};
use rand::{RngCore, SeedableRng};
use std::collections::HashMap;
use std::error::Error;
use std::sync::{Arc, Once};
use tokio::io::*;
use tokio::net::TcpStream;
use tokio::sync::{mpsc, Mutex};

#[derive(Debug)]
pub struct Client<T: SubListTrait> {
    pub srv: Arc<Mutex<ServerState<T>>>,
    pub cid: u64,
    pub msg_sender: Arc<Mutex<ClientMessageSender>>,
}
#[derive(Debug)]
pub struct ClientMessageSender {
    writer: WriteHalf<TcpStream>,
    msg_buf: Option<bytes::BytesMut>,
}
impl ClientMessageSender {
    pub fn new(writer: WriteHalf<TcpStream>) -> Self {
        Self {
            writer,
            msg_buf: Some(bytes::BytesMut::with_capacity(1024)),
        }
    }
}
impl<T: SubListTrait + Send + 'static> Client<T> {
    pub fn process_connection(
        cid: u64,
        srv: Arc<Mutex<ServerState<T>>>,
        conn: TcpStream,
    ) -> Arc<Mutex<ClientMessageSender>> {
        let (reader, writer) = tokio::io::split(conn);
        let msg_sender = Arc::new(Mutex::new(ClientMessageSender::new(writer)));
        let c = Client {
            srv: srv,
            cid,
            msg_sender: msg_sender.clone(),
        };
        tokio::spawn(Client::client_task(c, reader));
        msg_sender
    }
    async fn client_task(self, mut reader: ReadHalf<TcpStream>) {
        let mut parser = Parser::new();
        let mut count: i32 = 0;
        let mut subs = HashMap::new();
        loop {
            count += 1;
            let mut buf = [0; 1024 * 63];
            let r = reader.read(&mut buf[..]).await;
            if r.is_err() {
                let e = r.unwrap_err();
                self.process_error(e, subs).await;
                return;
            }
            let r = r.unwrap();
            let n = r;
            if n == 0 {
                self.process_error(NError::new(ERROR_CONNECTION_CLOSED), subs)
                    .await;
                return;
            }
            let mut buf2 = &buf[0..n];
            {
                let s = unsafe { std::str::from_utf8_unchecked(buf2) };
                if let Some(pos) = s.find("JJP") {
                    println!("n={},pos={},count={},receive err {}", n, pos, count, s);
                    std::process::exit(32);
                }
            }
            let mut rng = rand::rngs::StdRng::from_entropy();
            let mut cache = HashMap::new();
            loop {
                let r = parser.parse(&buf2[..]);
                if r.is_err() {
                    {
                        let s = unsafe { std::str::from_utf8_unchecked(&buf2[..]) };
                        println!("parse err buf={}", s);
                    }
                    self.process_error(r.unwrap_err(), subs).await;
                    return;
                }
                let (result, left) = r.unwrap();

                match result {
                    ParseResult::NoMsg => {
                        break;
                    }
                    ParseResult::Sub(ref sub) => {
                        if let Err(e) = self.process_sub(sub, &mut subs).await {
                            self.process_error(e, subs).await;
                            return;
                        }
                    }
                    ParseResult::Pub(ref pub_arg) => {
                        if let Err(e) = self.process_pub(pub_arg, &mut cache, &mut rng).await {
                            self.process_error(e, subs).await;
                            return;
                        }
                        parser.clear_msg_buf();
                    }
                }
                if left == buf2.len() {
                    break;
                }
                buf2 = &buf2[left..];
            }
        }
    }
    async fn process_error<E: Error>(&self, err: E, subs: HashMap<String, ArcSubscription>) {
        println!("client {} process err {:?}", self.cid, err);
        {
            let sublist = &mut self.srv.lock().await.sublist;
            for (_, sub) in subs {
                if let Err(e) = sublist.remove(sub) {
                    println!("client {} remove err {} ", self.cid, e);
                }
            }
        }
        let r = self.msg_sender.lock().await.writer.shutdown().await;
        if r.is_err() {
            println!("shutdown err {:?}", r.unwrap_err());
        }
    }
    async fn process_sub(
        &self,
        sub: &SubArg<'_>,
        subs: &mut HashMap<String, ArcSubscription>,
    ) -> crate::error::Result<()> {
        let sub = Subscription {
            subject: sub.subject.to_string(),
            queue: sub.queue.map(|q| q.to_string()),
            sid: sub.sid.to_string(),
            msg_sender: self.msg_sender.clone(),
        };
        let sub = Arc::new(sub);
        subs.insert(sub.subject.clone(), sub.clone());
        let sublist = &mut self.srv.lock().await.sublist;
        sublist.insert(sub)?;
        Ok(())
    }
    async fn process_pub(
        &self,
        pub_arg: &PubArg<'_>,
        cache: &mut HashMap<String, ArcSubResult>,
        rng: &mut rand::rngs::StdRng,
    ) -> crate::error::Result<()> {
        let sub_result = {
            if let Some(r) = cache.get(pub_arg.subject) {
                Arc::clone(r)
            } else {
                let sub_list = &mut self.srv.lock().await.sublist;
                let r = sub_list.match_subject(pub_arg.subject);
                cache.insert(pub_arg.subject.to_string(), Arc::clone(&r));
                r
            }
        };
        if sub_result.psubs.len() > 0 {
            let msg = Arc::new(Vec::from(pub_arg.msg));
            let (tx, mut rx) = mpsc::channel(sub_result.psubs.len());
            for sub in sub_result.psubs.iter() {
                let sub = Arc::clone(sub);
                let msg = msg.clone();
                let mut tx = tx.clone();
                tokio::spawn(async move {
                    if let Err(e) = Self::send_message2(sub, msg)
                        .await
                        .map_err(|_| NError::new(ERROR_CONNECTION_CLOSED))
                    {
                        println!("should close sub client");
                        //todo close sub client
                    }
                    if let Err(e) = tx.send(()).await {
                        panic!("never failed");
                    }
                });
            }
            for _ in 0..sub_result.psubs.len() {
                rx.recv().await;
            }
        }
        if sub_result.qsubs.len() > 0 {
            //qsubs 要考虑负载均衡问题
            for qsubs in sub_result.qsubs.iter() {
                let n = rng.next_u32();
                let n = n as usize % qsubs.len();
                let sub = qsubs.get(n).unwrap();
                self.send_message(sub.as_ref(), pub_arg)
                    .await
                    .map_err(|_| NError::new(ERROR_CONNECTION_CLOSED))?;
            }
        }
        Ok(())
    }
    ///消息格式
    ///```
    /// MSG <subject> <sid> <size>\r\n
    /// <message>\r\n
    /// ```
    async fn send_message(&self, sub: &Subscription, pub_arg: &PubArg<'_>) -> std::io::Result<()> {
        let mut msg_sender = sub.msg_sender.lock().await;
        let msg_buf = msg_sender.msg_buf.take().expect("must have");
        let writer = &mut msg_sender.writer;
        let mut buf = msg_buf.writer();
        buf.write("MSG ".as_bytes())?;
        buf.write(sub.subject.as_bytes())?;
        buf.write(" ".as_bytes())?;
        buf.write(sub.sid.as_bytes())?;
        buf.write(" ".as_bytes())?;
        buf.write(pub_arg.size_buf.as_bytes())?;
        buf.write("\r\n".as_bytes())?;
        buf.write(pub_arg.msg)?; //经测试,如果这里不使用缓存,而是多个await,性能会大幅下降.
        buf.write("\r\n".as_bytes())?;
        let mut msg_buf = buf.into_inner();
        writer.write(msg_buf.bytes()).await?;
        //        writer.flush().await?; 暂不需要flush,因为没有使用BufWriter
        msg_buf.clear();
        msg_sender.msg_buf = Some(msg_buf);
        Ok(())
    }
    async fn send_message2(sub: Arc<Subscription>, msg: Arc<Vec<u8>>) -> std::io::Result<()> {
        let mut msg_sender = sub.msg_sender.lock().await;
        let msg_buf = msg_sender.msg_buf.take().expect("must have");
        let writer = &mut msg_sender.writer;
        let mut buf = msg_buf.writer();
        buf.write("MSG ".as_bytes())?;
        buf.write(sub.subject.as_bytes())?;
        buf.write(" ".as_bytes())?;
        buf.write(sub.sid.as_bytes())?;
        buf.write(" ".as_bytes())?;
        write!(buf, "{}", msg.len())?;
        buf.write("\r\n".as_bytes())?;
        buf.write(msg.as_slice())?; //经测试,如果这里不使用缓存,而是多个await,性能会大幅下降.
        buf.write("\r\n".as_bytes())?;
        let mut msg_buf = buf.into_inner();
        writer.write(msg_buf.bytes()).await?;
        //        writer.flush().await?; 暂不需要flush,因为没有使用BufWriter
        msg_buf.clear();
        msg_sender.msg_buf = Some(msg_buf);
        Ok(())
    }
}
#[cfg(test)]
pub mod test_helper {
    use super::*;
    use lazy_static::lazy_static;
    lazy_static! {
        static ref SENDER: Arc<Mutex<ClientMessageSender>> = {
           let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        //    let port = l.local_addr().unwrap().port();
        let conn = std::net::TcpStream::connect(l.local_addr().unwrap()).unwrap();
        let (tx, rx) = std::sync::mpsc::channel();

        let rt: tokio::runtime::Runtime = tokio::runtime::Builder::new()
            .threaded_scheduler()
            .enable_all()
            .build()
            .unwrap();

        rt.spawn(async move {
            println!("tokio spawned");
            let conn = tokio::net::TcpStream::from_std(conn).unwrap();
            let (_, writer) = tokio::io::split(conn);
            println!("send start");
            let _=tx.send(writer);
            println!("send complete")
        });
        let writer = rx.recv().unwrap();
          Arc::new(Mutex::new(ClientMessageSender::new(writer)))
        };
    }
    #[cfg(test)]
    pub fn new_test_tcp_writer() -> Arc<Mutex<ClientMessageSender>> {
        SENDER.clone()
    }
}
use bytes::buf::BufMutExt;
use bytes::Buf;
use std::io::Write;
#[cfg(test)]
pub use test_helper::new_test_tcp_writer;

#[cfg(test)]
mod tests {
    use super::*;
    extern crate test;
    use test::Bencher;

    #[test]
    fn test() {}
    #[test]
    fn test_rng() {
        for _ in 0..10 {
            let mut r = rand::rngs::StdRng::from_entropy();
            println!("next={}", r.next_u32());
        }
    }
    #[bench]
    fn bench_gen_rng(b: &mut Bencher) {
        b.iter(|| {
            let mut r = rand::rngs::StdRng::from_entropy();
            drop(r);
        });
    }
}
