//! Multiplayer Snake game client. Creates or joins games over UDP, renders
//! the field and participant list in a terminal interface, and supports
//! seamless MASTER failover via DEPUTY election.

mod discovery;
mod engine;
mod game;
mod model;
mod net;
mod proto;
mod transport;
mod ui;

fn main() -> std::io::Result<()> {
    ui::run()
}
