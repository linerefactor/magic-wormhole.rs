// Manage connections to the Mailbox Server (which used to be known as the
// Rendezvous Server). The "Mailbox" machine specifically handles the mailbox
// object within that server, whereas this module manages the websocket
// connection (reconnecting after a delay when necessary), preliminary setup
// messages, and message packing/unpacking/dispatch.

// in Twisted, we delegate all of this to a ClientService, so there's a lot
// more code and more states here

use std::collections::VecDeque;
use super::traits::{TimerHandle, WSHandle, Action};

#[derive(Debug)]
enum State {
    Idle,
    Connecting,
    Connected,
    Waiting,
    Disconnecting, // -> Stopped
    Stopped,
}

#[derive(Debug)]
pub struct Rendezvous {
    wsh: WSHandle,
    relay_url: String,
    retry_timer: f32,
    appid: String,
    state: State,
    connected_at_least_once: bool,
    reconnect_timer: Option<TimerHandle>,
}

pub fn create(appid: &str, relay_url: &str, retry_timer: f32) -> Rendezvous {
    // we use a handle here just in case we need to open multiple connections
    // in the future. For now we ignore it, but the IO layer is supposed to
    // pass this back in websocket_* messages
    let wsh = WSHandle::new(1);
    Rendezvous {
        appid: appid.to_string(),
        relay_url: relay_url.to_string(),
        wsh: wsh,
        retry_timer: retry_timer,
        state: State::Idle,
        connected_at_least_once: false,
        reconnect_timer: None,
    }
}

impl Rendezvous {
    pub fn start(&mut self, actions: &mut VecDeque<Action>) -> () {
        // I want this to be stable, but that makes the lifetime weird
        //let wsh = self.wsh;
        //let wsh = WSHandle{};
        let newstate = match self.state {
            State::Idle => {
                let open = Action::WebSocketOpen(self.wsh,
                                                 self.relay_url.to_lowercase());
                //"url".to_string());
                actions.push_back(open);
                State::Connecting
            },
            _ => panic!("bad transition from {:?}", self),
        };
        self.state = newstate;
    }

    pub fn connection_made(&mut self,
                           actions: &mut VecDeque<Action>,
                           _handle: WSHandle) -> () {
        // TODO: assert handle == self.handle
        let newstate = match self.state {
            State::Connecting => {
                let bind = json!({"type": "bind",
                                  "appid": &self.appid,
                                  "side": "side1",
                                  });
                let bind = Action::WebSocketSendMessage(self.wsh,
                                                        bind.to_string());
                actions.push_back(bind);
                State::Connected
            },
            _ => panic!("bad transition from {:?}", self),
        };
        self.state = newstate;
    }

    pub fn connection_lost(&mut self,
                           actions: &mut VecDeque<Action>,
                           _handle: WSHandle) -> () {
        // TODO: assert handle == self.handle
        let newstate = match self.state {
            State::Connecting | State::Connected => {
                let new_handle = TimerHandle::new(2);
                self.reconnect_timer = Some(new_handle);
                // I.. don't know how to copy a String
                let wait = Action::StartTimer(new_handle, self.retry_timer);
                actions.push_back(wait);
                State::Waiting
            },
            State::Disconnecting => {
                State::Stopped
            },
            _ => panic!("bad transition from {:?}", self),
        };
        self.state = newstate;
    }

    pub fn timer_expired(&mut self,
                         actions: &mut VecDeque<Action>,
                         _handle: TimerHandle) -> () {
        // TODO: assert handle == self.handle
        let newstate = match self.state {
            State::Waiting => {
                let new_handle = WSHandle::new(2);
                // I.. don't know how to copy a String
                let open = Action::WebSocketOpen(new_handle,
                                                 self.relay_url.to_lowercase());
                actions.push_back(open);
                State::Connecting
            },
            _ => panic!("bad transition from {:?}", self),
        };
        self.state = newstate;
    }

    pub fn stop(&mut self,
                actions: &mut VecDeque<Action>) -> () {
        let newstate = match self.state {
            State::Idle | State::Stopped => {
                State::Stopped
            },
            State::Connecting | State::Connected => {
                let close = Action::WebSocketClose(self.wsh);
                actions.push_back(close);
                State::Disconnecting
            },
            State::Waiting => {
                let cancel = Action::CancelTimer(self.reconnect_timer.unwrap());
                actions.push_back(cancel);
                State::Stopped
            },
            State::Disconnecting => {
                State::Disconnecting
            },
        };
        self.state = newstate;
    }

}


#[cfg(test)]
mod test {
    use std::collections::VecDeque;
    use super::super::traits::Action;
    use super::super::traits::Action::{WebSocketOpen, StartTimer,
                                       WebSocketSendMessage};
    use super::super::traits::{WSHandle, TimerHandle};
    use serde_json;
    use serde_json::Value;

    #[test]
    fn create() {
        let mut actions: VecDeque<Action> = VecDeque::new();
        let mut r = super::create("appid", "url", 5.0);

        let mut wsh: WSHandle;
        let mut th: TimerHandle;

        r.start(&mut actions);

        match actions.pop_front() {
            Some(WebSocketOpen(handle, url)) => {
                assert_eq!(url, "url");
                wsh = handle;
            },
            _ => panic!(),
        }
        if let Some(_) = actions.pop_front() { panic!() };

        r.connection_made(&mut actions, wsh);
        match actions.pop_front() {
            Some(WebSocketSendMessage(handle, m)) => {
                //assert_eq!(handle, wsh);
                let b: Value = serde_json::from_str(&m).unwrap();
                assert_eq!(b["type"], "bind");
                assert_eq!(b["appid"], "appid");
                assert_eq!(b["side"], "side1");
            },
            _ => panic!(),
        }
        if let Some(_) = actions.pop_front() { panic!() };

        r.connection_lost(&mut actions, wsh);
        match actions.pop_front() {
            Some(StartTimer(handle, duration)) => {
                assert_eq!(duration, 5.0);
                th = handle;
            },
            _ => panic!(),
        }
        if let Some(_) = actions.pop_front() { panic!() };

        r.timer_expired(&mut actions, th);
        match actions.pop_front() {
            Some(WebSocketOpen(handle, url)) => {
                assert_eq!(url, "url");
                wsh = handle;
            },
            _ => panic!(),
        }
        if let Some(_) = actions.pop_front() { panic!() };

        r.stop(&mut actions);

    }
}