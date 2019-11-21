use core::future::{self, Future};
use core::marker::PhantomData;
use core::mem;
use core::pin::Pin;
use core::ptr;
use core::task::{Poll, Context, Waker, RawWaker, RawWakerVTable};

use alloc_collections::boxed::Box;
use alloc_collections::btree_map::BTreeMap;

use crate::interrupt::TrapFrame;
use crate::mem::MemoryExhausted;
use crate::mem::kalloc::GlobalAlloc;
use crate::page::{self, PageCtx};
use crate::sync::{Arc, Mutex};

pub const SEG_UCODE: u16 = 0x1b;
pub const SEG_UDATA: u16 = 0x23;

static TASKS: Mutex<Option<Tasks>> = Mutex::new(None);

#[derive(Debug, Clone, Copy, PartialOrd, PartialEq, Eq, Ord)]
pub struct TaskId(pub u64);

pub type TaskRef = Arc<Mutex<Task>>;

pub struct Tasks {
    map: BTreeMap<TaskId, TaskRef, GlobalAlloc>,
    current: Option<TaskRef>,
    next_id: u64,
}

#[derive(Debug)]
pub enum TaskState {
    Entry(TrapFrame),
    Wake,
    Sleep,
    User(TrapFrame),
}

type TaskFuture = Arc<Mutex<Pin<Box<dyn Future<Output = ()>, GlobalAlloc>>>>;

pub struct Task {
    id: TaskId,
    page_ctx: PageCtx,
    state: Arc<Mutex<TaskState>>,
    future: TaskFuture,
}

impl Tasks {
    pub fn create<F, Fut>(&mut self, page_ctx: PageCtx, f: F) -> Result<TaskRef, MemoryExhausted>
        where F: FnOnce(TaskHandle) -> Fut, Fut: Future<Output = ()> + 'static
    {
        let id = TaskId(self.next_id);
        self.next_id += 1;

        let task_state = Arc::new(Mutex::new(TaskState::Wake))?;

        let future = Box::new(f(TaskHandle {
            task_state: task_state.clone(),
        })).map_err(|_| MemoryExhausted)?;

        let future_obj = future as Box<dyn Future<Output = ()>, GlobalAlloc>;

        // TODO - why doesn't Pin::new work?
        let future_pin = unsafe { Pin::new_unchecked(future_obj) };

        let task = Arc::new(Mutex::new(Task {
            id,
            page_ctx,
            state: task_state,
            future: Arc::new(Mutex::new(future_pin))?,
        }))?;

        self.map.insert(id, task.clone())
            .map_err(|_| MemoryExhausted)?;

        Ok(task)
    }
}

pub unsafe fn start() -> Result<!, MemoryExhausted> {
    let mut tasks = Tasks {
        map: BTreeMap::new(),
        current: None,
        next_id: 1,
    };

    let init = tasks.create(page::current_ctx(), |mut task| async move {
        let mut frame = TrapFrame::new(0x1_0000_0000, 0x0);
        loop {
            let new_frame = task.step(frame).await;
            frame = new_frame;
        }
    })?;

    let second = tasks.create(page::current_ctx(), |mut task| async move {
        let mut frame = TrapFrame::new(0x1_0000_1000, 0x0);
        loop {
            let new_frame = task.step(frame).await;
            frame = new_frame;
        }
    })?;

    tasks.current = Some(init);

    *TASKS.lock() = Some(tasks);

    // begin:
    let mut frame = TrapFrame::new(0, 0);
    switch(&mut frame);
    asm!("
        movq $0, %rsp
        jmp interrupt_return
    " :: "r"(&mut frame as *mut TrapFrame) :: "volatile");

    loop {}
}

pub unsafe fn switch(frame: &mut TrapFrame) {
    fn save_current_task(frame: &mut TrapFrame) -> TaskId {
        let mut tasks = TASKS.lock();

        let tasks = tasks
            .as_mut()
            .expect("TASKS is not Some");

        // save old context
        match tasks.current {
            Some(ref task) => {
                let task = task.lock();
                let mut state = task.state.lock();
                match *state {
                    TaskState::User(ref mut task_frame) => {
                        *task_frame = frame.clone()
                    }
                    _ => {}
                }
                task.id
            }
            None => {
                TaskId(0)
            }
        }
    }

    enum WorkItem {
        Kernel(TaskFuture),
        User(TrapFrame),
    }

    fn find_next_work_item(previous_task_id: TaskId) -> (TaskId, WorkItem) {
        let mut tasks = TASKS.lock();

        let tasks = tasks
            .as_mut()
            .expect("TASKS is not Some");

        let next_tasks = tasks.map.range(previous_task_id..)
            .skip(1) // skip first task, it will always be `current_id`
            .chain(tasks.map.range(..=previous_task_id));

        for (id, task) in next_tasks {
            let task_locked = task.lock();
            let state = task_locked.state.lock();

            match *state {
                TaskState::Sleep => {
                    continue;
                }
                TaskState::Entry(_) | TaskState::Wake => {
                    tasks.current = Some(task.clone());

                    return (*id, WorkItem::Kernel(Arc::clone(&task_locked.future)));
                }
                TaskState::User(ref task_frame) => {
                    tasks.current = Some(task.clone());

                    return (*id, WorkItem::User(task_frame.clone()));
                }
            }
        }

        panic!("there should always be a task ready to run!");
    }

    let mut previous_task_id = save_current_task(frame);

    loop {
        match find_next_work_item(previous_task_id) {
            (new_task_id, WorkItem::Kernel(future)) => {
                let waker = Waker::from_raw(RawWaker::new(ptr::null(), &RAW_WAKER_VTABLE));
                let mut cx = Context::from_waker(&waker);
                let mut fut = future.lock();

                match fut.as_mut().poll(&mut cx) {
                    Poll::Ready(()) => panic!("task finished!"),
                    Poll::Pending => {}
                }

                previous_task_id = new_task_id;
            }
            (_, WorkItem::User(task_frame)) => {
                *frame = task_frame;
                return;
            }
        }
    }
}

pub unsafe fn dispatch_syscall(frame: &mut TrapFrame) {
    {
        let mut tasks = TASKS.lock();

        let mut current_task = tasks
            .as_mut().expect("TASKS is Some")
            .current
            .as_mut().expect("tasks.current is Some")
            .lock();

        let previous_state = mem::replace(
            &mut *current_task.state.lock(),
            TaskState::Entry(frame.clone()));

        match previous_state {
            TaskState::User(_) => { /* ok */ }
            _ => {
                panic!("syscall arrived from kernel context! task state: {:?}", previous_state);
            }
        }
    }

    // TODO don't switch immediately but process syscall on this task first:
    switch(frame)
}

static RAW_WAKER_VTABLE: RawWakerVTable = RawWakerVTable::new(
    waker_clone,
    waker_wake,
    waker_wake_by_ref,
    waker_drop,
);

unsafe fn waker_clone(_waker: *const ()) -> RawWaker {
    panic!("waker_clone");
}

unsafe fn waker_wake(_waker: *const ()) {
    panic!("waker_wake");
}

unsafe fn waker_wake_by_ref(_waker: *const ()) {
    panic!("waker_wake_by_ref");
}

unsafe fn waker_drop(_waker: *const ()) {}

pub struct TaskHandle {
    task_state: Arc<Mutex<TaskState>>,
}

impl TaskHandle {
    pub fn step(&mut self, frame: TrapFrame) -> Step {
        *self.task_state.lock() = TaskState::User(frame);
        Step {
            task_state: self.task_state.clone(),
            phantom: PhantomData,
        }
    }
}

pub struct Step<'a> {
    task_state: Arc<Mutex<TaskState>>,
    phantom: PhantomData<&'a mut TaskHandle>,
}

impl<'a> Future for Step<'a> {
    type Output = TrapFrame;

    fn poll(mut self: Pin<&mut Self>, cx: &mut core::task::Context) -> Poll<Self::Output> {
        match *self.task_state.lock() {
            TaskState::Entry(ref frame) => Poll::Ready(frame.clone()),
            TaskState::Wake => Poll::Pending,
            TaskState::User(_) => Poll::Pending,
            TaskState::Sleep => panic!("task state should not be Sleep"),
        }
    }
}
