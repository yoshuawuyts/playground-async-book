#![feature(arbitrary_self_types, async_await, await_macro, futures_api, pin)]

extern crate futures;

use futures::future::FutureObj;
use std::future::Future;
use std::pin::{Pin, Unpin};
use std::sync::mpsc::{sync_channel, Receiver, SyncSender};
use std::sync::{Arc, Mutex};
use std::task::{local_waker_from_nonlocal, Poll, Wake};
use std::task::{LocalWaker, Waker};
use std::thread;
use std::time::Duration;

/// Task executor that receives tasks off of a channel and runs them.
struct Executor {
  task_receiver: Receiver<Arc<Task>>,
}

/// `Spawner` spawns new futures onto the task channel.
#[derive(Clone)]
struct Spawner {
  task_sender: SyncSender<Arc<Task>>,
}

/// A future that can reschedule itself to be polled using a channel.
struct Task {
  // In-progress future that should be pushed to completion
  future: Mutex<Option<FutureObj<'static, ()>>>,
  // Handle to spawn tasks onto the task queue
  task_sender: SyncSender<Arc<Task>>,
}

fn new_executor_and_spawner() -> (Executor, Spawner) {
  // Maximum number of tasks to allow queueing in the channel at once.
  // This is just to make `sync_channel` happy, and wouldn't be present in
  // a real executor.
  const MAX_QUEUED_TASKS: usize = 10_000;
  let (task_sender, task_receiver) = sync_channel(MAX_QUEUED_TASKS);
  (Executor { task_receiver }, Spawner { task_sender })
}

impl Spawner {
  #[allow(parenthesized_params_in_types_and_modules)]
  fn spawn(&self, future: impl Future<Output = ()> + 'static + Send) {
    let future_obj = FutureObj::new(Box::new(future));

    let task = Arc::new(Task {
      future: Mutex::new(Some(future_obj)),
      task_sender: self.task_sender.clone(),
    });
    self.task_sender.send(task).expect("too many tasks queued");
  }
}

impl Wake for Task {
  fn wake(arc_self: &Arc<Self>) {
    // Implement `wake` by sending this task back onto the task channel
    // so that it will be polled again by the executor.
    let cloned = arc_self.clone();
    arc_self
      .task_sender
      .send(cloned)
      .expect("too many tasks queued");
  }
}

impl Executor {
  fn run(&self) {
    while let Ok(task) = self.task_receiver.recv() {
      let mut future_slot = task.future.lock().unwrap();
      // Take the future, and if it has not yet completed (is still Some),
      // poll it in an attempt to complete it.
      if let Some(mut future) = future_slot.take() {
        // Create a `LocalWaker` from the task itself
        let lw = local_waker_from_nonlocal(task.clone());
        if let Poll::Pending = Pin::new(&mut future).poll(&lw) {
          // We're not done processing the future, so put it
          // back in its task to be run again in the future.
          *future_slot = Some(future);
        }
      }
    }
  }
}

struct TimerFuture {
  shared_state: Arc<Mutex<SharedState>>,
}

/// Shared state between the future and the thread
struct SharedState {
  /// Whether or not the sleep time has elapsed
  completed: bool,

  /// The waker for the task that `TimerFuture` is running on.
  /// The thread can use this after setting `completed = true` to tell
  /// `TimerFuture`'s task to wake up, see that `completed = true`, and
  /// move forward.
  waker: Option<Waker>,
}

// Pinning will be covered later-- for now, it's enough to understand that our
// `TimerFuture` type doesn't require it, so it is `Unpin`.
impl Unpin for TimerFuture {}

impl Future for TimerFuture {
  type Output = ();
  fn poll(self: Pin<&mut Self>, lw: &LocalWaker) -> Poll<Self::Output> {
    // Look at the shared state to see if the timer has already completed.
    let mut shared_state = self.shared_state.lock().unwrap();
    if shared_state.completed {
      return Poll::Ready(());
    } else {
      shared_state.waker = Some(lw.clone().into_waker());
      return Poll::Pending;
    }
  }
}

impl TimerFuture {
  /// Create a new `TimerFuture` which will complete after the provided
  /// timeout.
  pub fn new(duration: Duration) -> Self {
    let shared_state = Arc::new(Mutex::new(SharedState {
      completed: false,
      waker: None,
    }));

    // Spawn the new thread
    let thread_shared_state = shared_state.clone();
    thread::spawn(move || {
      thread::sleep(duration);
      let mut shared_state = thread_shared_state.lock().unwrap();
      // Signal that the timer has completed and wake up the last
      // task on which the future was polled, if one exists.
      shared_state.completed = true;
      if let Some(waker) = &shared_state.waker {
        waker.wake();
      }
    });

    TimerFuture { shared_state }
  }
}

fn main() {
  let (executor, spawner) = new_executor_and_spawner();
  spawner.spawn(async {
    println!("before");
    await!(TimerFuture::new(Duration::new(2, 0)));
    println!("after");
  });

  executor.run();
}
