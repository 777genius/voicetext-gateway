use std::sync::atomic::{AtomicBool, Ordering};

use tokio::sync::{Mutex, mpsc};
use voicetext_speech::application::ports::{
    BoxFuture, LiveAudioFrame, LiveRecognitionEvent, LiveRecognitionRequest, LiveRecognizerFactory,
    LiveRecognizerSession, LiveTranscript, LiveTranscriptStability, RecognitionFailure,
};

#[derive(Debug)]
pub struct FakeLiveFactory;

impl LiveRecognizerFactory for FakeLiveFactory {
    fn open(
        &self,
        _request: LiveRecognitionRequest,
    ) -> BoxFuture<'_, Result<Box<dyn LiveRecognizerSession>, RecognitionFailure>> {
        let (sender, receiver) = mpsc::channel(8);
        let session = FakeLiveSession {
            sender,
            receiver: Mutex::new(receiver),
            emitted_transcript: AtomicBool::new(false),
        };
        Box::pin(async move { Ok(Box::new(session) as Box<dyn LiveRecognizerSession>) })
    }
}

#[derive(Debug)]
struct FakeLiveSession {
    sender: mpsc::Sender<LiveRecognitionEvent>,
    receiver: Mutex<mpsc::Receiver<LiveRecognitionEvent>>,
    emitted_transcript: AtomicBool,
}

impl LiveRecognizerSession for FakeLiveSession {
    fn write_audio(&self, _frame: LiveAudioFrame) -> BoxFuture<'_, Result<(), RecognitionFailure>> {
        Box::pin(async move {
            if !self.emitted_transcript.swap(true, Ordering::SeqCst) {
                self.sender
                    .send(LiveRecognitionEvent::Transcript(transcript(
                        LiveTranscriptStability::Partial,
                    )))
                    .await
                    .unwrap();
                self.sender
                    .send(LiveRecognitionEvent::Transcript(transcript(
                        LiveTranscriptStability::UtteranceFinal,
                    )))
                    .await
                    .unwrap();
            }
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

fn transcript(stability: LiveTranscriptStability) -> LiveTranscript {
    LiveTranscript {
        text: "synthetic live speech".into(),
        start_millis: 20,
        duration_millis: 40,
        confidence: Some(0.9),
        stability,
    }
}
