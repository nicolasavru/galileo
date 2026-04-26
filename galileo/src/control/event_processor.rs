use galileo_types::cartesian::{CartesianPoint2d, Point2};
use web_time::SystemTime;

use crate::control::{
    EventPropagation, MouseButton, MouseButtonsState, MouseEvent, RawUserEvent, TouchId, UserEvent,
    UserEventHandler,
};
use crate::map::Map;

const DRAG_THRESHOLD: f64 = 3.0;
const CLICK_TIMEOUT: std::time::Duration = std::time::Duration::from_millis(200);
const DBL_CLICK_TIMEOUT: std::time::Duration = std::time::Duration::from_millis(500);
// Touch-capable browsers (and some native platforms) deliver a touch
// sequence AND a synthesized mouse sequence for a single tap. After we
// emit a touch-derived Click we suppress any mouse-button Click that
// arrives within this window at approximately the same position, so
// downstream handlers see exactly one Click per tap.
const TOUCH_MOUSE_SUPPRESS_WINDOW: std::time::Duration = std::time::Duration::from_millis(500);
const TOUCH_MOUSE_SUPPRESS_DISTANCE: f64 = 25.0;

struct TouchInfo {
    id: TouchId,
    start_position: Point2,
    start_time: SystemTime,
    prev_position: Point2,
}

/// Stores input state, converts [`RawUserEvent`] into [`UserEvent`] and manages a list of event handlers.
///
/// When an even is called, the `EventProcessor` will go through event handlers one by one until a handler returns
/// [`EventPropagation::Consume`] or [`EventPropagation::Stop`]. At this point the event is considered to be handled.
pub struct EventProcessor {
    handlers: Vec<Box<dyn UserEventHandler>>,
    pointer_position: Point2,
    pointer_pressed_position: Point2,
    touches: Vec<TouchInfo>,

    buttons_state: MouseButtonsState,

    last_pressed_time: SystemTime,
    last_click_time: SystemTime,
    // Records the time and position of the most recent touch-derived
    // Click so we can suppress the synthetic mouse Click the platform
    // fires right after.
    last_touch_click: Option<(SystemTime, Point2)>,

    drag_target: Option<usize>,
}

impl Default for EventProcessor {
    fn default() -> Self {
        Self {
            handlers: vec![],
            pointer_position: Default::default(),
            pointer_pressed_position: Default::default(),
            touches: Vec::new(),
            buttons_state: Default::default(),
            last_pressed_time: SystemTime::UNIX_EPOCH,
            last_click_time: SystemTime::UNIX_EPOCH,
            last_touch_click: None,
            drag_target: None,
        }
    }
}

impl EventProcessor {
    /// Adds a new handler to the end of the handler list.
    pub fn add_handler(&mut self, handler: impl UserEventHandler + 'static) {
        self.handlers.push(Box::new(handler));
    }

    /// Adds a new handler to the end of the handler list.
    pub fn add_handler_boxed(&mut self, handler: Box<dyn UserEventHandler>) {
        self.handlers.push(handler);
    }

    /// Returns true if the processor is currently tracking dgragging by the pointer.
    pub fn is_dragging(&self) -> bool {
        self.drag_target.is_some()
    }

    /// Handles the event.
    pub fn handle(&mut self, event: RawUserEvent, map: &mut Map) {
        if let Some(user_events) = self.process(event) {
            for user_event in user_events {
                log::trace!("Handling user event: {user_event:?}");

                let mut drag_start_target = None;

                for (index, handler) in self.handlers.iter_mut().enumerate() {
                    if matches!(user_event, UserEvent::Drag(..) | UserEvent::DragEnded(..)) {
                        if let Some(target) = &self.drag_target {
                            if index != *target {
                                continue;
                            }
                        } else {
                            continue;
                        }
                    }

                    match handler.handle(&user_event, map) {
                        EventPropagation::Propagate => {}
                        EventPropagation::Stop => break,
                        EventPropagation::Consume => {
                            if let UserEvent::DragStarted(..) = user_event {
                                drag_start_target = Some(index);
                            }

                            break;
                        }
                    }
                }

                if drag_start_target.is_some() {
                    self.drag_target = drag_start_target;
                }

                if matches!(user_event, UserEvent::DragEnded(..)) {
                    self.drag_target = None;
                }
            }
        }
    }

    fn process(&mut self, event: RawUserEvent) -> Option<Vec<UserEvent>> {
        let now = SystemTime::now();
        match event {
            RawUserEvent::ButtonPressed(button) => {
                self.buttons_state.set_pressed(button);
                self.last_pressed_time = now;
                self.pointer_pressed_position = self.pointer_position;

                Some(vec![UserEvent::ButtonPressed(
                    button,
                    self.get_mouse_event(),
                )])
            }
            RawUserEvent::ButtonReleased(button) => {
                self.buttons_state.set_released(button);
                let mut events = vec![UserEvent::ButtonReleased(button, self.get_mouse_event())];

                if (now.duration_since(self.last_pressed_time)).unwrap_or_default() < CLICK_TIMEOUT
                {
                    // Browsers synthesize a Left mouse sequence right
                    // after a tap. If we just emitted a touch-derived
                    // Click at ~the same spot, drop this one — otherwise
                    // downstream handlers see two clicks per tap.
                    let is_synthetic_from_touch = button == MouseButton::Left
                        && self.last_touch_click.is_some_and(|(t, p)| {
                            now.duration_since(t).unwrap_or_default()
                                < TOUCH_MOUSE_SUPPRESS_WINDOW
                                && self
                                    .pointer_position
                                    .taxicab_distance(&p)
                                    < TOUCH_MOUSE_SUPPRESS_DISTANCE
                        });

                    if !is_synthetic_from_touch {
                        events.push(UserEvent::Click(button, self.get_mouse_event()));

                        if (now.duration_since(self.last_click_time)).unwrap_or_default()
                            < DBL_CLICK_TIMEOUT
                        {
                            events.push(UserEvent::DoubleClick(button, self.get_mouse_event()));
                        }

                        self.last_click_time = now;
                    }
                }

                if self.drag_target.is_some() {
                    events.push(UserEvent::DragEnded(button, self.get_mouse_event()));
                }

                Some(events)
            }
            RawUserEvent::PointerMoved(position) => {
                let prev_position = self.pointer_position;
                self.pointer_position = position;

                let mut events = vec![UserEvent::PointerMoved(self.get_mouse_event())];
                if let Some(button) = self.buttons_state.single_pressed() {
                    let mut is_dragging = self.drag_target.is_some();
                    if self.drag_target.is_none()
                        && position.taxicab_distance(&self.pointer_pressed_position)
                            > DRAG_THRESHOLD
                    {
                        events.push(UserEvent::DragStarted(
                            button,
                            self.get_mouse_event_pos(self.pointer_pressed_position),
                        ));

                        is_dragging = true;
                    }

                    if is_dragging {
                        events.push(UserEvent::Drag(
                            button,
                            self.pointer_position - prev_position,
                            self.get_mouse_event(),
                        ));
                    }
                }

                Some(events)
            }
            RawUserEvent::Scroll(delta) => {
                Some(vec![UserEvent::Scroll(delta, self.get_mouse_event())])
            }
            RawUserEvent::TouchStart(touch) => {
                for i in 0..self.touches.len() {
                    if self.touches[i].id == touch.touch_id {
                        // This should never happen, but in case it does, we don't wont a touch to be stuck here forever
                        self.touches.remove(i);
                        break;
                    }
                }

                self.touches.push(TouchInfo {
                    id: touch.touch_id,
                    start_position: touch.position,
                    start_time: now,
                    prev_position: touch.position,
                });

                None
            }
            RawUserEvent::TouchMove(touch) => {
                let touch_info = self.touches.iter().find(|t| t.id == touch.touch_id)?;
                let position = touch.position;

                let mut events = vec![];

                if self.touches.len() == 1 {
                    let mut is_dragging = self.drag_target.is_some();
                    if self.drag_target.is_none()
                        && position.taxicab_distance(&touch_info.start_position) > DRAG_THRESHOLD
                    {
                        events.push(UserEvent::DragStarted(
                            MouseButton::Other,
                            self.get_mouse_event_pos(touch_info.start_position),
                        ));

                        is_dragging = true
                    }

                    if is_dragging {
                        events.push(UserEvent::Drag(
                            MouseButton::Other,
                            position - touch_info.prev_position,
                            self.get_mouse_event_pos(position),
                        ));
                    }
                } else if self.touches.len() == 2 {
                    let Some(other_touch) = self.touches.iter().find(|t| t.id != touch_info.id)
                    else {
                        log::warn!("Unexpected touch id");
                        return None;
                    };

                    let distance = (other_touch.prev_position - position).magnitude();
                    let prev_distance =
                        (other_touch.prev_position - touch_info.prev_position).magnitude();
                    let zoom = prev_distance / distance;

                    events.push(UserEvent::Zoom(zoom, other_touch.prev_position))
                }

                for touch_info in &mut self.touches {
                    if touch_info.id == touch.touch_id {
                        touch_info.prev_position = position;
                    }
                }

                Some(events)
            }
            RawUserEvent::TouchEnd(touch) => {
                let mut ended_touch = None;
                for i in 0..self.touches.len() {
                    if self.touches[i].id == touch.touch_id {
                        ended_touch = Some(self.touches.remove(i));
                        break;
                    }
                }

                let mut events = vec![];

                if self.drag_target.is_some() && self.touches.is_empty() {
                    self.drag_target = None;
                    events.push(UserEvent::DragEnded(
                        MouseButton::Other,
                        self.get_mouse_event_pos(touch.position),
                    ));
                } else if let Some(ended_touch) = ended_touch {
                    // Require no concurrent touches so a pinch release doesn't fire Click.
                    let moved = touch
                        .position
                        .taxicab_distance(&ended_touch.start_position)
                        > DRAG_THRESHOLD;
                    let duration = now
                        .duration_since(ended_touch.start_time)
                        .unwrap_or_default();
                    if !moved
                        && duration < CLICK_TIMEOUT
                        && self.touches.is_empty()
                        && self.drag_target.is_none()
                    {
                        events.push(UserEvent::Click(
                            MouseButton::Other,
                            self.get_mouse_event_pos(touch.position),
                        ));

                        if now
                            .duration_since(self.last_click_time)
                            .unwrap_or_default()
                            < DBL_CLICK_TIMEOUT
                        {
                            events.push(UserEvent::DoubleClick(
                                MouseButton::Other,
                                self.get_mouse_event_pos(touch.position),
                            ));
                        }

                        self.last_click_time = now;
                        self.last_touch_click = Some((now, touch.position));
                    }
                }

                Some(events)
            }
        }
    }

    fn get_mouse_event(&self) -> MouseEvent {
        self.get_mouse_event_pos(self.pointer_position)
    }

    fn get_mouse_event_pos(&self, screen_pointer_position: Point2) -> MouseEvent {
        MouseEvent {
            screen_pointer_position,
            buttons: self.buttons_state,
        }
    }
}
