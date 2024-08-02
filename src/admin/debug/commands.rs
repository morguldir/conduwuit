use std::{
	collections::{BTreeMap, HashMap},
	fmt::Write,
	sync::{Arc, Mutex},
	time::{Instant, SystemTime},
};

use api::client::validate_and_add_event_id;
use conduit::{
	debug, debug_error, err, info, log,
	log::{capture, Capture},
	utils, warn, Error, PduEvent, Result,
};
use ruma::{
	api::{client::error::ErrorKind, federation::event::get_room_state},
	events::room::message::RoomMessageEventContent,
	CanonicalJsonObject, EventId, OwnedRoomOrAliasId, RoomId, RoomVersionId, ServerName,
};
use tokio::sync::RwLock;
use tracing_subscriber::EnvFilter;

use crate::admin_command;

#[admin_command]
pub(super) async fn echo(&self, message: Vec<String>) -> Result<RoomMessageEventContent> {
	let message = message.join(" ");

	Ok(RoomMessageEventContent::notice_plain(message))
}

#[admin_command]
pub(super) async fn get_auth_chain(&self, event_id: Box<EventId>) -> Result<RoomMessageEventContent> {
	let event_id = Arc::<EventId>::from(event_id);
	if let Some(event) = self.services.rooms.timeline.get_pdu_json(&event_id)? {
		let room_id_str = event
			.get("room_id")
			.and_then(|val| val.as_str())
			.ok_or_else(|| Error::bad_database("Invalid event in database"))?;

		let room_id = <&RoomId>::try_from(room_id_str)
			.map_err(|_| Error::bad_database("Invalid room id field in event in database"))?;

		let start = Instant::now();
		let count = self
			.services
			.rooms
			.auth_chain
			.event_ids_iter(room_id, vec![event_id])
			.await?
			.count();

		let elapsed = start.elapsed();
		Ok(RoomMessageEventContent::text_plain(format!(
			"Loaded auth chain with length {count} in {elapsed:?}"
		)))
	} else {
		Ok(RoomMessageEventContent::text_plain("Event not found."))
	}
}

#[admin_command]
pub(super) async fn parse_pdu(&self) -> Result<RoomMessageEventContent> {
	if self.body.len() < 2 || !self.body[0].trim().starts_with("```") || self.body.last().unwrap_or(&"").trim() != "```"
	{
		return Ok(RoomMessageEventContent::text_plain(
			"Expected code block in command body. Add --help for details.",
		));
	}

	let string = self.body[1..self.body.len().saturating_sub(1)].join("\n");
	match serde_json::from_str(&string) {
		Ok(value) => match ruma::signatures::reference_hash(&value, &RoomVersionId::V6) {
			Ok(hash) => {
				let event_id = EventId::parse(format!("${hash}"));

				match serde_json::from_value::<PduEvent>(serde_json::to_value(value).expect("value is json")) {
					Ok(pdu) => Ok(RoomMessageEventContent::text_plain(format!("EventId: {event_id:?}\n{pdu:#?}"))),
					Err(e) => Ok(RoomMessageEventContent::text_plain(format!(
						"EventId: {event_id:?}\nCould not parse event: {e}"
					))),
				}
			},
			Err(e) => Ok(RoomMessageEventContent::text_plain(format!("Could not parse PDU JSON: {e:?}"))),
		},
		Err(e) => Ok(RoomMessageEventContent::text_plain(format!(
			"Invalid json in command body: {e}"
		))),
	}
}

#[admin_command]
pub(super) async fn get_pdu(&self, event_id: Box<EventId>) -> Result<RoomMessageEventContent> {
	let mut outlier = false;
	let mut pdu_json = self
		.services
		.rooms
		.timeline
		.get_non_outlier_pdu_json(&event_id)?;
	if pdu_json.is_none() {
		outlier = true;
		pdu_json = self.services.rooms.timeline.get_pdu_json(&event_id)?;
	}
	match pdu_json {
		Some(json) => {
			let json_text = serde_json::to_string_pretty(&json).expect("canonical json is valid json");
			Ok(RoomMessageEventContent::notice_markdown(format!(
				"{}\n```json\n{}\n```",
				if outlier {
					"Outlier PDU found in our database"
				} else {
					"PDU found in our database"
				},
				json_text
			)))
		},
		None => Ok(RoomMessageEventContent::text_plain("PDU not found locally.")),
	}
}

#[admin_command]
pub(super) async fn get_remote_pdu_list(
	&self, server: Box<ServerName>, force: bool,
) -> Result<RoomMessageEventContent> {
	if !self.services.globals.config.allow_federation {
		return Ok(RoomMessageEventContent::text_plain(
			"Federation is disabled on this homeserver.",
		));
	}

	if server == self.services.globals.server_name() {
		return Ok(RoomMessageEventContent::text_plain(
			"Not allowed to send federation requests to ourselves. Please use `get-pdu` for fetching local PDUs from \
			 the database.",
		));
	}

	if self.body.len() < 2 || !self.body[0].trim().starts_with("```") || self.body.last().unwrap_or(&"").trim() != "```"
	{
		return Ok(RoomMessageEventContent::text_plain(
			"Expected code block in command body. Add --help for details.",
		));
	}

	let list = self
		.body
		.iter()
		.collect::<Vec<_>>()
		.drain(1..self.body.len().saturating_sub(1))
		.filter_map(|pdu| EventId::parse(pdu).ok())
		.collect::<Vec<_>>();

	let mut failed_count: usize = 0;
	let mut success_count: usize = 0;

	for pdu in list {
		if force {
			if let Err(e) = self.get_remote_pdu(Box::from(pdu), server.clone()).await {
				failed_count = failed_count.saturating_add(1);
				self.services
					.admin
					.send_message(RoomMessageEventContent::text_plain(format!(
						"Failed to get remote PDU, ignoring error: {e}"
					)))
					.await;
				warn!("Failed to get remote PDU, ignoring error: {e}");
			} else {
				success_count = success_count.saturating_add(1);
			}
		} else {
			self.get_remote_pdu(Box::from(pdu), server.clone()).await?;
			success_count = success_count.saturating_add(1);
		}
	}

	Ok(RoomMessageEventContent::text_plain(format!(
		"Fetched {success_count} remote PDUs successfully with {failed_count} failures"
	)))
}

#[admin_command]
pub(super) async fn get_remote_pdu(
	&self, event_id: Box<EventId>, server: Box<ServerName>,
) -> Result<RoomMessageEventContent> {
	if !self.services.globals.config.allow_federation {
		return Ok(RoomMessageEventContent::text_plain(
			"Federation is disabled on this homeserver.",
		));
	}

	if server == self.services.globals.server_name() {
		return Ok(RoomMessageEventContent::text_plain(
			"Not allowed to send federation requests to ourselves. Please use `get-pdu` for fetching local PDUs.",
		));
	}

	match self
		.services
		.sending
		.send_federation_request(
			&server,
			ruma::api::federation::event::get_event::v1::Request {
				event_id: event_id.clone().into(),
			},
		)
		.await
	{
		Ok(response) => {
			let json: CanonicalJsonObject = serde_json::from_str(response.pdu.get()).map_err(|e| {
				warn!(
					"Requested event ID {event_id} from server but failed to convert from RawValue to \
					 CanonicalJsonObject (malformed event/response?): {e}"
				);
				Error::BadRequest(ErrorKind::Unknown, "Received response from server but failed to parse PDU")
			})?;

			debug!("Attempting to parse PDU: {:?}", &response.pdu);
			let parsed_pdu = {
				let parsed_result = self
					.services
					.rooms
					.event_handler
					.parse_incoming_pdu(&response.pdu);
				let (event_id, value, room_id) = match parsed_result {
					Ok(t) => t,
					Err(e) => {
						warn!("Failed to parse PDU: {e}");
						info!("Full PDU: {:?}", &response.pdu);
						return Ok(RoomMessageEventContent::text_plain(format!(
							"Failed to parse PDU remote server {server} sent us: {e}"
						)));
					},
				};

				vec![(event_id, value, room_id)]
			};

			let pub_key_map = RwLock::new(BTreeMap::new());

			debug!("Attempting to fetch homeserver signing keys for {server}");
			self.services
				.server_keys
				.fetch_required_signing_keys(parsed_pdu.iter().map(|(_event_id, event, _room_id)| event), &pub_key_map)
				.await
				.unwrap_or_else(|e| {
					warn!("Could not fetch all signatures for PDUs from {server}: {e:?}");
				});

			info!("Attempting to handle event ID {event_id} as backfilled PDU");
			self.services
				.rooms
				.timeline
				.backfill_pdu(&server, response.pdu, &pub_key_map)
				.await?;

			let json_text = serde_json::to_string_pretty(&json).expect("canonical json is valid json");

			Ok(RoomMessageEventContent::notice_markdown(format!(
				"{}\n```json\n{}\n```",
				"Got PDU from specified server and handled as backfilled PDU successfully. Event body:", json_text
			)))
		},
		Err(e) => Ok(RoomMessageEventContent::text_plain(format!(
			"Remote server did not have PDU or failed sending request to remote server: {e}"
		))),
	}
}

#[admin_command]
pub(super) async fn get_room_state(&self, room: OwnedRoomOrAliasId) -> Result<RoomMessageEventContent> {
	let room_id = self.services.rooms.alias.resolve(&room).await?;
	let room_state = self
		.services
		.rooms
		.state_accessor
		.room_state_full(&room_id)
		.await?
		.values()
		.map(|pdu| pdu.to_state_event())
		.collect::<Vec<_>>();

	if room_state.is_empty() {
		return Ok(RoomMessageEventContent::text_plain(
			"Unable to find room state in our database (vector is empty)",
		));
	}

	let json = serde_json::to_string_pretty(&room_state).map_err(|e| {
		warn!("Failed converting room state vector in our database to pretty JSON: {e}");
		Error::bad_database(
			"Failed to convert room state events to pretty JSON, possible invalid room state events in our database",
		)
	})?;

	Ok(RoomMessageEventContent::notice_markdown(format!("```json\n{json}\n```")))
}

#[admin_command]
pub(super) async fn ping(&self, server: Box<ServerName>) -> Result<RoomMessageEventContent> {
	if server == self.services.globals.server_name() {
		return Ok(RoomMessageEventContent::text_plain(
			"Not allowed to send federation requests to ourselves.",
		));
	}

	let timer = tokio::time::Instant::now();

	match self
		.services
		.sending
		.send_federation_request(&server, ruma::api::federation::discovery::get_server_version::v1::Request {})
		.await
	{
		Ok(response) => {
			let ping_time = timer.elapsed();

			let json_text_res = serde_json::to_string_pretty(&response.server);

			if let Ok(json) = json_text_res {
				return Ok(RoomMessageEventContent::notice_markdown(format!(
					"Got response which took {ping_time:?} time:\n```json\n{json}\n```"
				)));
			}

			Ok(RoomMessageEventContent::text_plain(format!(
				"Got non-JSON response which took {ping_time:?} time:\n{response:?}"
			)))
		},
		Err(e) => {
			warn!("Failed sending federation request to specified server from ping debug command: {e}");
			Ok(RoomMessageEventContent::text_plain(format!(
				"Failed sending federation request to specified server:\n\n{e}",
			)))
		},
	}
}

#[admin_command]
pub(super) async fn force_device_list_updates(&self) -> Result<RoomMessageEventContent> {
	// Force E2EE device list updates for all users
	for user_id in self.services.users.iter().filter_map(Result::ok) {
		self.services.users.mark_device_key_update(&user_id)?;
	}
	Ok(RoomMessageEventContent::text_plain(
		"Marked all devices for all users as having new keys to update",
	))
}

#[admin_command]
pub(super) async fn change_log_level(&self, filter: Option<String>, reset: bool) -> Result<RoomMessageEventContent> {
	let handles = &["console"];

	if reset {
		let old_filter_layer = match EnvFilter::try_new(&self.services.globals.config.log) {
			Ok(s) => s,
			Err(e) => {
				return Ok(RoomMessageEventContent::text_plain(format!(
					"Log level from config appears to be invalid now: {e}"
				)));
			},
		};

		match self
			.services
			.server
			.log
			.reload
			.reload(&old_filter_layer, Some(handles))
		{
			Ok(()) => {
				return Ok(RoomMessageEventContent::text_plain(format!(
					"Successfully changed log level back to config value {}",
					self.services.globals.config.log
				)));
			},
			Err(e) => {
				return Ok(RoomMessageEventContent::text_plain(format!(
					"Failed to modify and reload the global tracing log level: {e}"
				)));
			},
		}
	}

	if let Some(filter) = filter {
		let new_filter_layer = match EnvFilter::try_new(filter) {
			Ok(s) => s,
			Err(e) => {
				return Ok(RoomMessageEventContent::text_plain(format!(
					"Invalid log level filter specified: {e}"
				)));
			},
		};

		match self
			.services
			.server
			.log
			.reload
			.reload(&new_filter_layer, Some(handles))
		{
			Ok(()) => {
				return Ok(RoomMessageEventContent::text_plain("Successfully changed log level"));
			},
			Err(e) => {
				return Ok(RoomMessageEventContent::text_plain(format!(
					"Failed to modify and reload the global tracing log level: {e}"
				)));
			},
		}
	}

	Ok(RoomMessageEventContent::text_plain("No log level was specified."))
}

#[admin_command]
pub(super) async fn sign_json(&self) -> Result<RoomMessageEventContent> {
	if self.body.len() < 2 || !self.body[0].trim().starts_with("```") || self.body.last().unwrap_or(&"").trim() != "```"
	{
		return Ok(RoomMessageEventContent::text_plain(
			"Expected code block in command body. Add --help for details.",
		));
	}

	let string = self.body[1..self.body.len().checked_sub(1).unwrap()].join("\n");
	match serde_json::from_str(&string) {
		Ok(mut value) => {
			ruma::signatures::sign_json(
				self.services.globals.server_name().as_str(),
				self.services.globals.keypair(),
				&mut value,
			)
			.expect("our request json is what ruma expects");
			let json_text = serde_json::to_string_pretty(&value).expect("canonical json is valid json");
			Ok(RoomMessageEventContent::text_plain(json_text))
		},
		Err(e) => Ok(RoomMessageEventContent::text_plain(format!("Invalid json: {e}"))),
	}
}

#[admin_command]
pub(super) async fn verify_json(&self) -> Result<RoomMessageEventContent> {
	if self.body.len() < 2 || !self.body[0].trim().starts_with("```") || self.body.last().unwrap_or(&"").trim() != "```"
	{
		return Ok(RoomMessageEventContent::text_plain(
			"Expected code block in command body. Add --help for details.",
		));
	}

	let string = self.body[1..self.body.len().checked_sub(1).unwrap()].join("\n");
	match serde_json::from_str(&string) {
		Ok(value) => {
			let pub_key_map = RwLock::new(BTreeMap::new());

			self.services
				.server_keys
				.fetch_required_signing_keys([&value], &pub_key_map)
				.await?;

			let pub_key_map = pub_key_map.read().await;
			match ruma::signatures::verify_json(&pub_key_map, &value) {
				Ok(()) => Ok(RoomMessageEventContent::text_plain("Signature correct")),
				Err(e) => Ok(RoomMessageEventContent::text_plain(format!(
					"Signature verification failed: {e}"
				))),
			}
		},
		Err(e) => Ok(RoomMessageEventContent::text_plain(format!("Invalid json: {e}"))),
	}
}

#[admin_command]
#[tracing::instrument(skip(self))]
pub(super) async fn first_pdu_in_room(&self, room_id: Box<RoomId>) -> Result<RoomMessageEventContent> {
	if !self
		.services
		.rooms
		.state_cache
		.server_in_room(&self.services.globals.config.server_name, &room_id)?
	{
		return Ok(RoomMessageEventContent::text_plain(
			"We are not participating in the room / we don't know about the room ID.",
		));
	}

	let first_pdu = self
		.services
		.rooms
		.timeline
		.first_pdu_in_room(&room_id)?
		.ok_or_else(|| Error::bad_database("Failed to find the first PDU in database"))?;

	Ok(RoomMessageEventContent::text_plain(format!("{first_pdu:?}")))
}

#[admin_command]
#[tracing::instrument(skip(self))]
pub(super) async fn latest_pdu_in_room(&self, room_id: Box<RoomId>) -> Result<RoomMessageEventContent> {
	if !self
		.services
		.rooms
		.state_cache
		.server_in_room(&self.services.globals.config.server_name, &room_id)?
	{
		return Ok(RoomMessageEventContent::text_plain(
			"We are not participating in the room / we don't know about the room ID.",
		));
	}

	let latest_pdu = self
		.services
		.rooms
		.timeline
		.latest_pdu_in_room(&room_id)?
		.ok_or_else(|| Error::bad_database("Failed to find the latest PDU in database"))?;

	Ok(RoomMessageEventContent::text_plain(format!("{latest_pdu:?}")))
}

#[admin_command]
#[tracing::instrument(skip(self))]
pub(super) async fn force_set_room_state_from_server(
	&self, room_id: Box<RoomId>, server_name: Box<ServerName>,
) -> Result<RoomMessageEventContent> {
	if !self
		.services
		.rooms
		.state_cache
		.server_in_room(&self.services.globals.config.server_name, &room_id)?
	{
		return Ok(RoomMessageEventContent::text_plain(
			"We are not participating in the room / we don't know about the room ID.",
		));
	}

	let first_pdu = self
		.services
		.rooms
		.timeline
		.latest_pdu_in_room(&room_id)?
		.ok_or_else(|| Error::bad_database("Failed to find the latest PDU in database"))?;

	let room_version = self.services.rooms.state.get_room_version(&room_id)?;

	let mut state: HashMap<u64, Arc<EventId>> = HashMap::new();
	let pub_key_map = RwLock::new(BTreeMap::new());

	let remote_state_response = self
		.services
		.sending
		.send_federation_request(
			&server_name,
			get_room_state::v1::Request {
				room_id: room_id.clone().into(),
				event_id: first_pdu.event_id.clone().into(),
			},
		)
		.await?;

	let mut events = Vec::with_capacity(remote_state_response.pdus.len());

	for pdu in remote_state_response.pdus.clone() {
		events.push(match self.services.rooms.event_handler.parse_incoming_pdu(&pdu) {
			Ok(t) => t,
			Err(e) => {
				warn!("Could not parse PDU, ignoring: {e}");
				continue;
			},
		});
	}

	info!("Fetching required signing keys for all the state events we got");
	self.services
		.server_keys
		.fetch_required_signing_keys(events.iter().map(|(_event_id, event, _room_id)| event), &pub_key_map)
		.await?;

	info!("Going through room_state response PDUs");
	for result in remote_state_response
		.pdus
		.iter()
		.map(|pdu| validate_and_add_event_id(self.services, pdu, &room_version, &pub_key_map))
	{
		let Ok((event_id, value)) = result.await else {
			continue;
		};

		let pdu = PduEvent::from_id_val(&event_id, value.clone()).map_err(|e| {
			debug_error!("Invalid PDU in fetching remote room state PDUs response: {value:#?}");
			err!(BadServerResponse(debug_error!("Invalid PDU in send_join response: {e:?}")))
		})?;

		self.services
			.rooms
			.outlier
			.add_pdu_outlier(&event_id, &value)?;
		if let Some(state_key) = &pdu.state_key {
			let shortstatekey = self
				.services
				.rooms
				.short
				.get_or_create_shortstatekey(&pdu.kind.to_string().into(), state_key)?;
			state.insert(shortstatekey, pdu.event_id.clone());
		}
	}

	info!("Going through auth_chain response");
	for result in remote_state_response
		.auth_chain
		.iter()
		.map(|pdu| validate_and_add_event_id(self.services, pdu, &room_version, &pub_key_map))
	{
		let Ok((event_id, value)) = result.await else {
			continue;
		};

		self.services
			.rooms
			.outlier
			.add_pdu_outlier(&event_id, &value)?;
	}

	let new_room_state = self
		.services
		.rooms
		.event_handler
		.resolve_state(room_id.clone().as_ref(), &room_version, state)
		.await?;

	info!("Forcing new room state");
	let (short_state_hash, new, removed) = self
		.services
		.rooms
		.state_compressor
		.save_state(room_id.clone().as_ref(), new_room_state)?;

	let state_lock = self.services.rooms.state.mutex.lock(&room_id).await;
	self.services
		.rooms
		.state
		.force_state(room_id.clone().as_ref(), short_state_hash, new, removed, &state_lock)
		.await?;

	info!(
		"Updating joined counts for room just in case (e.g. we may have found a difference in the room's \
		 m.room.member state"
	);
	self.services
		.rooms
		.state_cache
		.update_joined_count(&room_id)?;

	drop(state_lock);

	Ok(RoomMessageEventContent::text_plain(
		"Successfully forced the room state from the requested remote server.",
	))
}

#[admin_command]
pub(super) async fn get_signing_keys(
	&self, server_name: Option<Box<ServerName>>, _cached: bool,
) -> Result<RoomMessageEventContent> {
	let server_name = server_name.unwrap_or_else(|| self.services.server.config.server_name.clone().into());
	let signing_keys = self.services.globals.signing_keys_for(&server_name)?;

	Ok(RoomMessageEventContent::notice_markdown(format!(
		"```rs\n{signing_keys:#?}\n```"
	)))
}

#[admin_command]
#[allow(dead_code)]
pub(super) async fn get_verify_keys(
	&self, server_name: Option<Box<ServerName>>, cached: bool,
) -> Result<RoomMessageEventContent> {
	let server_name = server_name.unwrap_or_else(|| self.services.server.config.server_name.clone().into());
	let mut out = String::new();

	if cached {
		writeln!(out, "| Key ID | VerifyKey |")?;
		writeln!(out, "| --- | --- |")?;
		for (key_id, verify_key) in self.services.globals.verify_keys_for(&server_name)? {
			writeln!(out, "| {key_id} | {verify_key:?} |")?;
		}

		return Ok(RoomMessageEventContent::notice_markdown(out));
	}

	let signature_ids: Vec<String> = Vec::new();
	let keys = self
		.services
		.server_keys
		.fetch_signing_keys_for_server(&server_name, signature_ids)
		.await?;

	writeln!(out, "| Key ID | Public Key |")?;
	writeln!(out, "| --- | --- |")?;
	for (key_id, key) in keys {
		writeln!(out, "| {key_id} | {key} |")?;
	}

	Ok(RoomMessageEventContent::notice_markdown(out))
}

#[admin_command]
pub(super) async fn resolve_true_destination(
	&self, server_name: Box<ServerName>, no_cache: bool,
) -> Result<RoomMessageEventContent> {
	if !self.services.globals.config.allow_federation {
		return Ok(RoomMessageEventContent::text_plain(
			"Federation is disabled on this homeserver.",
		));
	}

	if server_name == self.services.globals.config.server_name {
		return Ok(RoomMessageEventContent::text_plain(
			"Not allowed to send federation requests to ourselves. Please use `get-pdu` for fetching local PDUs.",
		));
	}

	let filter: &capture::Filter = &|data| {
		data.level() <= log::Level::DEBUG
			&& data.mod_name().starts_with("conduit")
			&& matches!(data.span_name(), "actual" | "well-known" | "srv")
	};

	let state = &self.services.server.log.capture;
	let logs = Arc::new(Mutex::new(String::new()));
	let capture = Capture::new(state, Some(filter), capture::fmt_markdown(logs.clone()));

	let capture_scope = capture.start();
	let actual = self
		.services
		.resolver
		.resolve_actual_dest(&server_name, !no_cache)
		.await?;
	drop(capture_scope);

	let msg = format!(
		"{}\nDestination: {}\nHostname URI: {}",
		logs.lock().expect("locked"),
		actual.dest,
		actual.host,
	);
	Ok(RoomMessageEventContent::text_markdown(msg))
}

#[admin_command]
pub(super) async fn memory_stats(&self) -> Result<RoomMessageEventContent> {
	let html_body = conduit::alloc::memory_stats();

	if html_body.is_none() {
		return Ok(RoomMessageEventContent::text_plain(
			"malloc stats are not supported on your compiled malloc.",
		));
	}

	Ok(RoomMessageEventContent::text_html(
		"This command's output can only be viewed by clients that render HTML.".to_owned(),
		html_body.expect("string result"),
	))
}

#[cfg(tokio_unstable)]
#[admin_command]
pub(super) async fn runtime_metrics(&self) -> Result<RoomMessageEventContent> {
	let out = self.services.server.metrics.runtime_metrics().map_or_else(
		|| "Runtime metrics are not available.".to_owned(),
		|metrics| format!("```rs\n{metrics:#?}\n```"),
	);

	Ok(RoomMessageEventContent::text_markdown(out))
}

#[cfg(not(tokio_unstable))]
#[admin_command]
pub(super) async fn runtime_metrics(&self) -> Result<RoomMessageEventContent> {
	Ok(RoomMessageEventContent::text_markdown(
		"Runtime metrics require building with `tokio_unstable`.",
	))
}

#[cfg(tokio_unstable)]
#[admin_command]
pub(super) async fn runtime_interval(&self) -> Result<RoomMessageEventContent> {
	let out = self.services.server.metrics.runtime_interval().map_or_else(
		|| "Runtime metrics are not available.".to_owned(),
		|metrics| format!("```rs\n{metrics:#?}\n```"),
	);

	Ok(RoomMessageEventContent::text_markdown(out))
}

#[cfg(not(tokio_unstable))]
#[admin_command]
pub(super) async fn runtime_interval(&self) -> Result<RoomMessageEventContent> {
	Ok(RoomMessageEventContent::text_markdown(
		"Runtime metrics require building with `tokio_unstable`.",
	))
}

#[admin_command]
pub(super) async fn time(&self) -> Result<RoomMessageEventContent> {
	let now = SystemTime::now();
	Ok(RoomMessageEventContent::text_markdown(utils::time::format(now, "%+")))
}

#[admin_command]
pub(super) async fn list_dependencies(&self, names: bool) -> Result<RoomMessageEventContent> {
	if names {
		let out = info::cargo::dependencies_names().join(" ");
		return Ok(RoomMessageEventContent::notice_markdown(out));
	}

	let deps = info::cargo::dependencies();
	let mut out = String::new();
	writeln!(out, "| name | version | features |")?;
	writeln!(out, "| ---- | ------- | -------- |")?;
	for (name, dep) in deps {
		let version = dep.try_req().unwrap_or("*");
		let feats = dep.req_features();
		let feats = if !feats.is_empty() {
			feats.join(" ")
		} else {
			String::new()
		};
		writeln!(out, "{name} | {version} | {feats}")?;
	}

	Ok(RoomMessageEventContent::notice_markdown(out))
}
