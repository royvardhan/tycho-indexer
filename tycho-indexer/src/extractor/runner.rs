use anyhow::{format_err, Context, Result};
use async_trait::async_trait;
use prost::Message;
use std::{collections::HashMap, env, sync::Arc};
use tokio::{
    sync::{
        mpsc::{self, error::SendError, Receiver, Sender},
        Mutex,
    },
    task::JoinHandle,
};
use tokio_stream::StreamExt;
use tracing::{debug, error, info, instrument, trace, warn, Instrument};

use super::{Extractor, ExtractorMsg};
use crate::{
    extractor::ExtractionError,
    models::ExtractorIdentity,
    pb::sf::substreams::v1::Package,
    substreams::{
        stream::{BlockResponse, SubstreamsStream},
        SubstreamsEndpoint,
    },
};

pub enum ControlMessage {
    Stop,
    Subscribe(Sender<ExtractorMsg>),
}

/// A trait for a message sender that can be used to subscribe to messages
///
/// Extracted out of the [ExtractorHandle] to allow for easier testing
#[async_trait]
pub trait MessageSender: Send + Sync {
    async fn subscribe(&self) -> Result<Receiver<ExtractorMsg>, SendError<ControlMessage>>;
}

#[derive(Clone)]
pub struct ExtractorHandle {
    id: ExtractorIdentity,
    control_tx: Sender<ControlMessage>,
}

impl ExtractorHandle {
    fn new(id: ExtractorIdentity, control_tx: Sender<ControlMessage>) -> Self {
        Self { id, control_tx }
    }

    pub fn get_id(&self) -> ExtractorIdentity {
        self.id.clone()
    }

    #[instrument(skip(self))]
    pub async fn stop(&self) -> Result<(), ExtractionError> {
        // TODO: send a oneshot along here and wait for it
        self.control_tx
            .send(ControlMessage::Stop)
            .await
            .map_err(|err| ExtractionError::Unknown(err.to_string()))
    }
}

#[async_trait]
impl MessageSender for ExtractorHandle {
    #[instrument(skip(self))]
    async fn subscribe(&self) -> Result<Receiver<ExtractorMsg>, SendError<ControlMessage>> {
        let (tx, rx) = mpsc::channel(1);
        self.control_tx
            .send(ControlMessage::Subscribe(tx))
            .await?;

        Ok(rx)
    }
}

// Define the SubscriptionsMap type alias
type SubscriptionsMap = HashMap<u64, Sender<ExtractorMsg>>;

pub struct ExtractorRunner {
    extractor: Arc<dyn Extractor>,
    substreams: SubstreamsStream,
    subscriptions: Arc<Mutex<SubscriptionsMap>>,
    next_subscriber_id: u64,
    control_rx: Receiver<ControlMessage>,
}

impl ExtractorRunner {
    pub fn new(
        extractor: Arc<dyn Extractor>,
        substreams: SubstreamsStream,
        subscriptions: Arc<Mutex<SubscriptionsMap>>,
        control_rx: Receiver<ControlMessage>,
    ) -> Self {
        ExtractorRunner { extractor, substreams, subscriptions, next_subscriber_id: 0, control_rx }
    }
    pub fn run(mut self) -> JoinHandle<Result<(), ExtractionError>> {
        let id = self.extractor.get_id().clone();

        tokio::spawn(async move {
            let id = self.extractor.get_id();
            loop {
                tokio::select! {
                    Some(ctrl) = self.control_rx.recv() =>  {
                        match ctrl {
                            ControlMessage::Stop => {
                                warn!("Stop signal received; exiting!");
                                return Ok(())
                            },
                            ControlMessage::Subscribe(sender) => {
                                self.subscribe(sender).await;
                            },
                        }
                    }
                    val = self.substreams.next() => {
                        match val {
                            None => {
                                return Err(ExtractionError::SubstreamsError(format!("{}: stream ended", id)));
                            }
                            Some(Ok(BlockResponse::New(data))) => {
                                let block_number = data.clock.as_ref().map(|v| v.number).unwrap_or(0);
                                tracing::Span::current().record("block_number", block_number);
                                debug!("New block data received.");
                                match self.extractor.handle_tick_scoped_data(data).await {
                                    Ok(Some(msg)) => {
                                        trace!("Propagating new block data message.");
                                        Self::propagate_msg(&self.subscriptions, msg).await
                                    }
                                    Ok(None) => {
                                        trace!("No message to propagate.");
                                    }
                                    Err(err) => {
                                        error!(error = %err, "Error while processing tick!");
                                        return Err(err);
                                    }
                                }
                            }
                            Some(Ok(BlockResponse::Undo(undo_signal))) => {
                                let block = undo_signal.last_valid_block.as_ref().map(|v| v.number).unwrap_or(0);
                                warn!(%block,  "Revert to {block} requested!");
                                match self.extractor.handle_revert(undo_signal).await {
                                    Ok(Some(msg)) => {
                                        trace!(msg = %msg, "Propagating block undo message.");
                                        Self::propagate_msg(&self.subscriptions, msg).await
                                    }
                                    Ok(None) => {
                                        trace!("No message to propagate.");
                                    }
                                    Err(err) => {
                                        error!(error = %err, "Error while processing revert!");
                                        return Err(err);
                                    }
                                }
                            }
                            Some(Err(err)) => {
                                error!(error = %err, "Stream terminated with error.");
                                return Err(ExtractionError::SubstreamsError(err.to_string()));
                            }
                        };
                    }
                }
            }
        }
        .instrument(tracing::info_span!("extractor_runner::run", id = %id)))
    }

    #[instrument(skip_all)]
    async fn subscribe(&mut self, sender: Sender<ExtractorMsg>) {
        let subscriber_id = self.next_subscriber_id;
        self.next_subscriber_id += 1;
        tracing::Span::current().record("subscriber_id", subscriber_id);
        info!("New subscription with id {}", subscriber_id);
        self.subscriptions
            .lock()
            .await
            .insert(subscriber_id, sender);
    }

    // TODO: add message tracing_id to the log
    #[instrument(skip_all)]
    async fn propagate_msg(subscribers: &Arc<Mutex<SubscriptionsMap>>, message: ExtractorMsg) {
        info!(msg = %message, "Propagating message to subscribers.");
        // TODO: rename variable here instead
        let arced_message = message;

        let mut to_remove = Vec::new();

        // Lock the subscribers HashMap for exclusive access
        let mut subscribers = subscribers.lock().await;

        for (counter, sender) in subscribers.iter_mut() {
            match sender.send(arced_message.clone()).await {
                Ok(_) => {
                    // Message sent successfully
                    debug!(subscriber_id = %counter, "Message sent successfully.");
                }
                Err(err) => {
                    // Receiver has been dropped, mark for removal
                    to_remove.push(*counter);
                    error!(error = %err, "Error while sending message to subscriber {}", counter);
                }
            }
        }

        // Remove inactive subscribers
        for counter in to_remove {
            subscribers.remove(&counter);
            debug!("Subscriber {} has been dropped", counter);
        }
    }
}

pub struct ExtractorRunnerBuilder {
    spkg_file: String,
    endpoint_url: String,
    module_name: String,
    start_block: i64,
    end_block: i64,
    token: String,
    extractor: Arc<dyn Extractor>,
}

pub type HandleResult = (JoinHandle<Result<(), ExtractionError>>, ExtractorHandle);

impl ExtractorRunnerBuilder {
    pub fn new(spkg: &str, extractor: Arc<dyn Extractor>) -> Self {
        Self {
            spkg_file: spkg.to_owned(),
            endpoint_url: "https://mainnet.eth.streamingfast.io:443".to_owned(),
            module_name: "map_changes".to_owned(),
            start_block: 0,
            end_block: 0,
            token: env::var("SUBSTREAMS_API_TOKEN").unwrap_or("".to_string()),
            extractor,
        }
    }

    #[allow(dead_code)]
    pub fn endpoint_url(mut self, val: &str) -> Self {
        self.endpoint_url = val.to_owned();
        self
    }

    pub fn module_name(mut self, val: &str) -> Self {
        self.module_name = val.to_owned();
        self
    }

    pub fn start_block(mut self, val: i64) -> Self {
        self.start_block = val;
        self
    }

    pub fn end_block(mut self, val: i64) -> Self {
        self.end_block = val;
        self
    }

    #[allow(dead_code)]
    pub fn token(mut self, val: &str) -> Self {
        self.token = val.to_owned();
        self
    }

    #[instrument(skip(self))]
    pub async fn run(self) -> Result<HandleResult, ExtractionError> {
        let content = std::fs::read(&self.spkg_file)
            .context(format_err!("read package from file '{}'", self.spkg_file))
            .map_err(|err| ExtractionError::SubstreamsError(err.to_string()))?;
        let spkg = Package::decode(content.as_ref())
            .context("decode command")
            .map_err(|err| ExtractionError::SubstreamsError(err.to_string()))?;
        let endpoint = Arc::new(
            SubstreamsEndpoint::new(&self.endpoint_url, Some(self.token))
                .await
                .map_err(|err| ExtractionError::SubstreamsError(err.to_string()))?,
        );
        let cursor = self.extractor.get_cursor().await;
        let stream = SubstreamsStream::new(
            endpoint,
            Some(cursor),
            spkg.modules.clone(),
            self.module_name,
            self.start_block,
            self.end_block as u64,
        );

        let id = self.extractor.get_id();
        let (ctrl_tx, ctrl_rx) = mpsc::channel(1);
        let runner = ExtractorRunner::new(
            self.extractor,
            stream,
            Arc::new(Mutex::new(HashMap::new())),
            ctrl_rx,
        );

        let handle = runner.run();
        Ok((handle, ExtractorHandle::new(id, ctrl_tx)))
    }
}

#[cfg(test)]
mod test {
    use serde::{Deserialize, Serialize};
    use tracing::info_span;

    use crate::{extractor::MockExtractor, models::NormalisedMessage};

    use super::*;

    #[derive(Clone, Debug, PartialEq, Eq, Hash, Deserialize, Serialize)]
    struct DummyMessage {
        extractor_id: ExtractorIdentity,
    }

    impl std::fmt::Display for DummyMessage {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            write!(f, "{}", self.extractor_id)
        }
    }

    impl DummyMessage {
        pub fn new(extractor_id: ExtractorIdentity) -> Self {
            Self { extractor_id }
        }
    }

    #[typetag::serde]
    impl NormalisedMessage for DummyMessage {
        fn source(&self) -> ExtractorIdentity {
            self.extractor_id.clone()
        }
    }

    pub struct MyMessageSender {
        extractor_id: ExtractorIdentity,
    }

    impl MyMessageSender {
        #[allow(dead_code)]
        pub fn new(extractor_id: ExtractorIdentity) -> Self {
            Self { extractor_id }
        }
    }

    #[async_trait]
    impl MessageSender for MyMessageSender {
        async fn subscribe(&self) -> Result<Receiver<ExtractorMsg>, SendError<ControlMessage>> {
            let (tx, rx) = mpsc::channel::<ExtractorMsg>(1);
            let extractor_id = self.extractor_id.clone();

            // Spawn a task that sends a DummyMessage every 100ms
            tokio::spawn(async move {
                loop {
                    tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;
                    debug!("Sending DummyMessage");
                    let dummy_message = DummyMessage::new(extractor_id.clone());
                    if tx
                        .send(Arc::new(dummy_message))
                        .await
                        .is_err()
                    {
                        debug!("Receiver dropped");
                        break;
                    }
                }
                .instrument(info_span!("DummyMessageSender", extractor_id = %extractor_id))
            });

            Ok(rx)
        }
    }

    #[tokio::test]
    async fn test_extractor_runner_builder() {
        // Mock the Extractor
        let mut mock_extractor = MockExtractor::new();
        mock_extractor
            .expect_get_cursor()
            .returning(|| "cursor@0".to_string());
        mock_extractor
            .expect_get_id()
            .returning(ExtractorIdentity::default);

        // Build the ExtractorRunnerBuilder
        let extractor = Arc::new(mock_extractor);
        let builder = ExtractorRunnerBuilder::new(
            "./test/spkg/substreams-ethereum-quickstart-v1.0.0.spkg",
            extractor,
        )
        .endpoint_url("https://mainnet.eth.streamingfast.io:443")
        .module_name("test_module")
        .start_block(0)
        .end_block(10)
        .token("test_token");

        // Run the builder
        let (task, _handle) = builder.run().await.unwrap();

        // Wait for the handle to complete
        match task.await {
            Ok(_) => {
                info!("ExtractorRunnerBuilder completed successfully");
            }
            Err(err) => {
                error!(error = %err, "ExtractorRunnerBuilder failed");
                panic!("ExtractorRunnerBuilder failed");
            }
        }
    }
}
