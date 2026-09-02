use std::sync::atomic::{AtomicUsize, Ordering};
use voicetext_gateway::contracts::live::LiveIdentity;

use tokio::sync::{Mutex, mpsc};
use voicetext_speech::application::ports::{
    BoxFuture, LiveAudioFrame, LiveRecognitionEvent, LiveRecognitionRequest, LiveRecognizerFactory,
    LiveRecognizerSession, LiveTranscript, LiveTranscriptStability, RecognitionFailure,
};

#[derive(Debug)]
pub struct FakeLiveFactory(pub LiveIdentity);

impl LiveRecognizerFactory for FakeLiveFactory {
    fn capabilities(
        &self,
    ) -> &'static voicetext_speech::application::live_capabilities::LiveCapabilityDescriptor {
        super::live_capabilities(self.0)
    }

    fn open(
        &self,
        _request: LiveRecognitionRequest,
    ) -> BoxFuture<'_, Result<Box<dyn LiveRecognizerSession>, RecognitionFailure>> {
        let (sender, receiver) = mpsc::channel(8);
        let session = FakeLiveSession {
            sender,
            receiver: Mutex::new(receiver),
            accepted_audio: AtomicUsize::new(0),
        };
        Box::pin(async move { Ok(Box::new(session) as Box<dyn LiveRecognizerSession>) })
    }
}

#[derive(Debug)]
struct FakeLiveSession {
    sender: mpsc::Sender<LiveRecognitionEvent>,
    receiver: Mutex<mpsc::Receiver<LiveRecognitionEvent>>,
    accepted_audio: AtomicUsize,
}

impl LiveRecognizerSession for FakeLiveSession {
    fn write_audio(&self, _frame: LiveAudioFrame) -> BoxFuture<'_, Result<(), RecognitionFailure>> {
        Box::pin(async move {
            let accepted_audio = self.accepted_audio.fetch_add(1, Ordering::SeqCst);
            if accepted_audio == 0 {
                self.sender
                    .send(LiveRecognitionEvent::Transcript(transcript(
                        LiveTranscriptStability::Partial,
                        accepted_audio,
                    )))
                    .await
                    .unwrap();
            }
            self.sender
                .send(LiveRecognitionEvent::Transcript(transcript(
                    LiveTranscriptStability::UtteranceFinal,
                    accepted_audio,
                )))
                .await
                .unwrap();
            Ok(())
        })
    }

    fn next_event(
        &self,
    ) -> BoxFuture<'_, Result<Option<LiveRecognitionEvent>, RecognitionFailure>> {
        Box::pin(async move { Ok(self.receiver.lock().await.recv().await) })
    }

    fn finalize(&self) -> BoxFuture<'_, Result<(), RecognitionFailure>> {
        Box::pin(async move {
            self.sender
                .send(LiveRecognitionEvent::FinalizeResultObserved)
                .await
                .unwrap();
            Ok(())
        })
    }

    fn close(&self) -> BoxFuture<'_, Result<(), RecognitionFailure>> {
        Box::pin(async { Ok(()) })
    }
}

fn transcript(stability: LiveTranscriptStability, accepted_audio: usize) -> LiveTranscript {
    LiveTranscript {
        text: "synthetic live speech".into(),
        start_millis: 20
            + u64::try_from(accepted_audio)
                .unwrap_or(u64::MAX)
                .saturating_mul(40),
        duration_millis: 40,
        confidence: Some(0.9),
        stability,
    }
}
