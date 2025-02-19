use bevy::ecs::event::ManualEventReader;
use bevy::ecs::system::SystemParam;
use bevy::prelude::*;
use bevy::tasks::AsyncComputeTaskPool;
use futures_util::future::join;
use futures_util::{select, FutureExt, SinkExt, StreamExt};
use game::{
    AnyGameEvent, GameEvent, PieceConnectionCheckEvent, PieceConnectionEvent, PieceMovedEvent,
    PiecePickedUpEvent, PiecePutDownEvent, PlayerCursorMovedEvent, PlayerDisconnectedEvent, Puzzle,
};
use tokio::sync::mpsc::error::TryRecvError;
use tokio::sync::oneshot;
use ws_stream_wasm::{WsMessage, WsMeta};

use crate::states::AppState;
use crate::ui::LoadingMessage;
use crate::worker::Worker;

pub struct NetworkPlugin;

impl Plugin for NetworkPlugin {
    fn build(&self, app: &mut App) {
        app.add_systems(OnEnter(AppState::Connecting), spawn_network_io_task)
            .add_systems(
                Update,
                download_puzzle.run_if(in_state(AppState::Downloading)),
            )
            .add_systems(Update, event_io.run_if(in_state(AppState::Playing)));
    }
}

type NetworkIO = Worker<String, String>;

fn spawn_network_io_task(
    mut commands: Commands,
    mut next_state: ResMut<NextState<AppState>>,
    mut loading_msg: ResMut<LoadingMessage>,
) {
    let thread_pool = AsyncComputeTaskPool::get();
    let io = NetworkIO::spawn(thread_pool, |mut client_rx, client_tx| async move {
        let window = web_sys::window().unwrap();
        let document = window.document().unwrap();
        let location = document.location().unwrap();
        let host = location.host().unwrap();

        let ws_address = if cfg!(debug_assertions) {
            format!("ws://{host}/client")
        } else {
            format!("wss://{host}/client")
        };

        let ws_io = match WsMeta::connect(ws_address.as_str(), None).await {
            Ok((_, ws_io)) => ws_io,
            Err(_) => {
                return;
            }
        };

        let (mut ws_tx, mut ws_rx) = ws_io.split();
        let (dc_tx, dc_rx) = oneshot::channel();

        let net_rx_handler = async move {
            let mut disconnect = dc_rx.fuse();
            loop {
                select! {
                    _ = disconnect => {
                        break;
                    },
                    res = ws_rx.next().fuse() => match res {
                        None => break,
                        Some(msg) => match msg {
                            WsMessage::Text(msg) => client_tx.send(msg).unwrap(),
                            WsMessage::Binary(msg) => warn!("Strange message received from server: {msg:#?}"),
                        }
                    },
                }
            }
        };

        let net_tx_handler = async move {
            while let Some(msg) = client_rx.recv().await {
                if ws_tx.send(WsMessage::Text(msg)).await.is_err() {
                    break;
                }
            }
            let _ = dc_tx.send(());
        };

        join(net_rx_handler, net_tx_handler).await;
    });
    commands.insert_resource(io);
    next_state.set(AppState::Downloading);
    loading_msg.0 = String::from("Connecting to server");
}

fn download_puzzle(
    mut commands: Commands,
    mut network_io: ResMut<NetworkIO>,
    mut next_state: ResMut<NextState<AppState>>,
) {
    match network_io.output.try_recv() {
        Ok(msg) => {
            if let Ok(puzzle) = Puzzle::deserialize(msg.as_str()) {
                commands.insert_resource(puzzle);
                next_state.set(AppState::Cutting);
            } else {
                warn!("Unexpected message from server while waiting for puzzle: {msg:#?}");
            }
        }
        Err(e) => match e {
            TryRecvError::Empty => (),
            TryRecvError::Disconnected => next_state.set(AppState::Connecting),
        },
    }
}

#[derive(SystemParam)]
struct EventIoParams<'w, 's> {
    piece_moved_events: ResMut<'w, Events<PieceMovedEvent>>,
    piece_moved_reader: Local<'s, ManualEventReader<PieceMovedEvent>>,

    piece_picked_up_events: ResMut<'w, Events<PiecePickedUpEvent>>,
    piece_picked_up_reader: Local<'s, ManualEventReader<PiecePickedUpEvent>>,

    piece_put_down_events: ResMut<'w, Events<PiecePutDownEvent>>,
    piece_put_down_reader: Local<'s, ManualEventReader<PiecePutDownEvent>>,

    piece_connection_check_events: ResMut<'w, Events<PieceConnectionCheckEvent>>,
    piece_connection_check_reader: Local<'s, ManualEventReader<PieceConnectionCheckEvent>>,

    piece_connection_events: ResMut<'w, Events<PieceConnectionEvent>>,
    piece_connection_reader: Local<'s, ManualEventReader<PieceConnectionEvent>>,

    player_cursor_moved_events: ResMut<'w, Events<PlayerCursorMovedEvent>>,
    player_cursor_moved_reader: Local<'s, ManualEventReader<PlayerCursorMovedEvent>>,

    player_disconnected_events: ResMut<'w, Events<PlayerDisconnectedEvent>>,
    player_disconnected_reader: Local<'s, ManualEventReader<PlayerDisconnectedEvent>>,
}

fn event_io(
    mut params: EventIoParams,
    mut network_io: ResMut<NetworkIO>,
    mut puzzle: ResMut<Puzzle>,
    mut next_state: ResMut<NextState<AppState>>,
) {
    // forward all events generated by the client to the server

    macro_rules! forward_events {
        ($reader: ident, $events: ident) => {
            for event in params.$reader.iter(&params.$events) {
                if network_io.input.send(event.serialize()).is_err() {
                    next_state.set(AppState::Connecting);
                    return;
                }
            }
        };
    }

    forward_events!(piece_moved_reader, piece_moved_events);
    forward_events!(piece_picked_up_reader, piece_picked_up_events);
    forward_events!(piece_put_down_reader, piece_put_down_events);
    forward_events!(piece_connection_check_reader, piece_connection_check_events);
    forward_events!(piece_connection_reader, piece_connection_events);
    forward_events!(player_cursor_moved_reader, player_cursor_moved_events);
    forward_events!(player_disconnected_reader, player_disconnected_events);

    // receive events from the server and apply them to the local puzzle instance
    let mut new_events = Vec::new();
    while let Ok(msg) = network_io.output.try_recv() {
        let event = AnyGameEvent::deserialize(msg.as_str()).unwrap();
        new_events.extend(puzzle.apply_event(event));
    }

    // dispatch new events out to bevy
    for event in new_events {
        use AnyGameEvent::*;
        match event {
            PieceMoved(event) => params.piece_moved_events.send(event),
            PiecePickedUp(event) => params.piece_picked_up_events.send(event),
            PiecePutDown(event) => params.piece_put_down_events.send(event),
            PieceConnectionCheck(event) => params.piece_connection_check_events.send(event),
            PieceConnection(event) => params.piece_connection_events.send(event),
            PlayerCursorMoved(event) => params.player_cursor_moved_events.send(event),
            PlayerDisconnected(event) => params.player_disconnected_events.send(event),
        }
    }

    // consume all the events we just dispatched so we don't forward them back out next frame
    params.piece_moved_reader.clear(&params.piece_moved_events);
    params
        .piece_picked_up_reader
        .clear(&params.piece_picked_up_events);
    params
        .piece_put_down_reader
        .clear(&params.piece_put_down_events);
    params
        .piece_connection_check_reader
        .clear(&params.piece_connection_check_events);
    params
        .piece_connection_reader
        .clear(&params.piece_connection_events);
    params
        .player_cursor_moved_reader
        .clear(&params.player_cursor_moved_events);
    params
        .player_disconnected_reader
        .clear(&params.player_disconnected_events);
}
