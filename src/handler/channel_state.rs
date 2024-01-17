use crate::client::Client;
use crate::error::MumbleError;
use crate::handler::Handler;
use crate::proto::mumble::ChannelState;
use crate::proto::MessageKind;
use crate::sync::RwLock;
use crate::ServerState;
use async_trait::async_trait;
use std::sync::Arc;

#[async_trait]
impl Handler for ChannelState {
    async fn handle(&self, state: Arc<RwLock<ServerState>>, client: Arc<RwLock<Client>>) -> Result<(), MumbleError> {
        if self.has_channel_id() {
            tracing::warn!("editing channel is not supported");

            return Ok(());
        }

        if !self.has_parent() {
            tracing::warn!("cannot create channel: channel must have a parent");

            return Ok(());
        }

        if !self.has_name() {
            tracing::warn!("cannot create channel: channel must have a name");

            return Ok(());
        }

        let name = self.get_name();

        if !{ state.read_err().await?.channels.contains_key(&self.get_parent()) } {
            tracing::warn!("cannot create channel: parent channel does not exist");

            return Ok(());
        }

        let existing_channel = { state.read_err().await?.get_channel_by_name(name).await? };

        let new_channel_id = if let Some(channel) = existing_channel {
            let channel_state = { channel.read_err().await?.get_channel_state() };

            {
                client
                    .read_err()
                    .await?
                    .send_message(MessageKind::ChannelState, &channel_state)
                    .await?;
            }

            channel_state.get_channel_id()
        } else {
            let channel = { state.write_err().await?.add_channel(self) };
            let channel_state = { channel.read_err().await?.get_channel_state() };

            {
                state
                    .read_err()
                    .await?
                    .broadcast_message(MessageKind::ChannelState, &channel_state)
                    .await?;
            }

            channel_state.get_channel_id()
        };

        let leave_channel_id_result = { state.read_err().await?.set_client_channel(client.clone(), new_channel_id).await };

        let leave_channel_id = match leave_channel_id_result {
            Ok(Some(leave_channel_id)) => leave_channel_id,
            Ok(None) => return Ok(()),
            Err(_) => return Ok(()),
        };

        {
            state.write_err().await?.channels.remove(&leave_channel_id);
        }

        Ok(())
    }
}
