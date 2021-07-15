use crate::{
    client_server::invite_helper, database::DatabaseGuard, pdu::PduBuilder, ConduitResult, Error,
    Ruma,
};
use log::info;
use ruma::{
    api::client::{
        error::ErrorKind,
        r0::room::{self, create_room, get_room_event, upgrade_room},
    },
    events::{
        room::{
            guest_access, history_visibility, join_rules, member,
            name::{self, RoomName},
            topic,
        },
        EventType,
    },
    serde::Raw,
    RoomAliasId, RoomId, RoomVersionId,
};
use std::{cmp::max, collections::BTreeMap, convert::TryFrom, sync::Arc};

#[cfg(feature = "conduit_bin")]
use rocket::{get, post};

#[cfg_attr(
    feature = "conduit_bin",
    post("/_matrix/client/r0/createRoom", data = "<body>")
)]
#[tracing::instrument(skip(db, body))]
pub async fn create_room_route(
    db: DatabaseGuard,
    body: Ruma<create_room::Request<'_>>,
) -> ConduitResult<create_room::Response> {
    let sender_user = body.sender_user.as_ref().expect("user is authenticated");

    let room_id = RoomId::new(db.globals.server_name());

    let mutex = Arc::clone(
        db.globals
            .roomid_mutex
            .write()
            .unwrap()
            .entry(room_id.clone())
            .or_default(),
    );
    let mutex_lock = mutex.lock().await;

    let alias = body
        .room_alias_name
        .as_ref()
        .map_or(Ok(None), |localpart| {
            // TODO: Check for invalid characters and maximum length
            let alias =
                RoomAliasId::try_from(format!("#{}:{}", localpart, db.globals.server_name()))
                    .map_err(|_| Error::BadRequest(ErrorKind::InvalidParam, "Invalid alias."))?;

            if db.rooms.id_from_alias(&alias)?.is_some() {
                Err(Error::BadRequest(
                    ErrorKind::RoomInUse,
                    "Room alias already exists.",
                ))
            } else {
                Ok(Some(alias))
            }
        })?;

    let mut content = ruma::events::room::create::CreateEventContent::new(sender_user.clone());
    content.federate = body.creation_content.federate;
    content.predecessor = body.creation_content.predecessor.clone();
    content.room_version = RoomVersionId::Version6;

    // 1. The room create event
    db.rooms.build_and_append_pdu(
        PduBuilder {
            event_type: EventType::RoomCreate,
            content: serde_json::to_value(content).expect("event is valid, we just created it"),
            unsigned: None,
            state_key: Some("".to_owned()),
            redacts: None,
        },
        &sender_user,
        &room_id,
        &db,
        &mutex_lock,
    )?;

    // 2. Let the room creator join
    db.rooms.build_and_append_pdu(
        PduBuilder {
            event_type: EventType::RoomMember,
            content: serde_json::to_value(member::MemberEventContent {
                membership: member::MembershipState::Join,
                displayname: db.users.displayname(&sender_user)?,
                avatar_url: db.users.avatar_url(&sender_user)?,
                is_direct: Some(body.is_direct),
                third_party_invite: None,
                blurhash: db.users.blurhash(&sender_user)?,
            })
            .expect("event is valid, we just created it"),
            unsigned: None,
            state_key: Some(sender_user.to_string()),
            redacts: None,
        },
        &sender_user,
        &room_id,
        &db,
        &mutex_lock,
    )?;

    // 3. Power levels

    // Figure out preset. We need it for preset specific events
    let preset = body
        .preset
        .clone()
        .unwrap_or_else(|| match &body.visibility {
            room::Visibility::Private => create_room::RoomPreset::PrivateChat,
            room::Visibility::Public => create_room::RoomPreset::PublicChat,
            _ => create_room::RoomPreset::PrivateChat, // Room visibility should not be custom
        });

    let mut users = BTreeMap::new();
    users.insert(sender_user.clone(), 100.into());

    if preset == create_room::RoomPreset::TrustedPrivateChat {
        for invite_ in &body.invite {
            users.insert(invite_.clone(), 100.into());
        }
    }

    let mut power_levels_content =
        serde_json::to_value(ruma::events::room::power_levels::PowerLevelsEventContent {
            users,
            ..Default::default()
        })
        .expect("event is valid, we just created it");

    if let Some(power_level_content_override) = &body.power_level_content_override {
        let json = serde_json::from_str::<serde_json::Map<String, serde_json::Value>>(
            power_level_content_override.json().get(),
        )
        .map_err(|_| {
            Error::BadRequest(ErrorKind::BadJson, "Invalid power_level_content_override.")
        })?;

        for (key, value) in json {
            power_levels_content[key] = value;
        }
    }

    db.rooms.build_and_append_pdu(
        PduBuilder {
            event_type: EventType::RoomPowerLevels,
            content: power_levels_content,
            unsigned: None,
            state_key: Some("".to_owned()),
            redacts: None,
        },
        &sender_user,
        &room_id,
        &db,
        &mutex_lock,
    )?;

    // 4. Events set by preset

    // 4.1 Join Rules
    db.rooms.build_and_append_pdu(
        PduBuilder {
            event_type: EventType::RoomJoinRules,
            content: match preset {
                create_room::RoomPreset::PublicChat => serde_json::to_value(
                    join_rules::JoinRulesEventContent::new(join_rules::JoinRule::Public),
                )
                .expect("event is valid, we just created it"),
                // according to spec "invite" is the default
                _ => serde_json::to_value(join_rules::JoinRulesEventContent::new(
                    join_rules::JoinRule::Invite,
                ))
                .expect("event is valid, we just created it"),
            },
            unsigned: None,
            state_key: Some("".to_owned()),
            redacts: None,
        },
        &sender_user,
        &room_id,
        &db,
        &mutex_lock,
    )?;

    // 4.2 History Visibility
    db.rooms.build_and_append_pdu(
        PduBuilder {
            event_type: EventType::RoomHistoryVisibility,
            content: serde_json::to_value(history_visibility::HistoryVisibilityEventContent::new(
                history_visibility::HistoryVisibility::Shared,
            ))
            .expect("event is valid, we just created it"),
            unsigned: None,
            state_key: Some("".to_owned()),
            redacts: None,
        },
        &sender_user,
        &room_id,
        &db,
        &mutex_lock,
    )?;

    // 4.3 Guest Access
    db.rooms.build_and_append_pdu(
        PduBuilder {
            event_type: EventType::RoomGuestAccess,
            content: match preset {
                create_room::RoomPreset::PublicChat => {
                    serde_json::to_value(guest_access::GuestAccessEventContent::new(
                        guest_access::GuestAccess::Forbidden,
                    ))
                    .expect("event is valid, we just created it")
                }
                _ => serde_json::to_value(guest_access::GuestAccessEventContent::new(
                    guest_access::GuestAccess::CanJoin,
                ))
                .expect("event is valid, we just created it"),
            },
            unsigned: None,
            state_key: Some("".to_owned()),
            redacts: None,
        },
        &sender_user,
        &room_id,
        &db,
        &mutex_lock,
    )?;

    // 5. Events listed in initial_state
    for event in &body.initial_state {
        let pdu_builder = serde_json::from_str::<PduBuilder>(
            &serde_json::to_string(&event).expect("AnyInitialStateEvent::to_string always works"),
        )
        .map_err(|_| Error::BadRequest(ErrorKind::InvalidParam, "Invalid initial state event."))?;

        // Silently skip encryption events if they are not allowed
        if pdu_builder.event_type == EventType::RoomEncryption && !db.globals.allow_encryption() {
            continue;
        }

        db.rooms
            .build_and_append_pdu(pdu_builder, &sender_user, &room_id, &db, &mutex_lock)?;
    }

    // 6. Events implied by name and topic
    if let Some(name) = &body.name {
        db.rooms.build_and_append_pdu(
            PduBuilder {
                event_type: EventType::RoomName,
                content: serde_json::to_value(name::NameEventContent::new(Some(
                    RoomName::try_from(name.clone()).map_err(|_| {
                        Error::BadRequest(ErrorKind::InvalidParam, "Name is invalid.")
                    })?,
                )))
                .expect("event is valid, we just created it"),
                unsigned: None,
                state_key: Some("".to_owned()),
                redacts: None,
            },
            &sender_user,
            &room_id,
            &db,
            &mutex_lock,
        )?;
    }

    if let Some(topic) = &body.topic {
        db.rooms.build_and_append_pdu(
            PduBuilder {
                event_type: EventType::RoomTopic,
                content: serde_json::to_value(topic::TopicEventContent {
                    topic: topic.clone(),
                })
                .expect("event is valid, we just created it"),
                unsigned: None,
                state_key: Some("".to_owned()),
                redacts: None,
            },
            &sender_user,
            &room_id,
            &db,
            &mutex_lock,
        )?;
    }

    // 7. Events implied by invite (and TODO: invite_3pid)
    drop(mutex_lock);
    for user_id in &body.invite {
        let _ = invite_helper(sender_user, user_id, &room_id, &db, body.is_direct).await;
    }

    // Homeserver specific stuff
    if let Some(alias) = alias {
        db.rooms.set_alias(&alias, Some(&room_id), &db.globals)?;
    }

    if body.visibility == room::Visibility::Public {
        db.rooms.set_public(&room_id, true)?;
    }

    info!("{} created a room", sender_user);

    db.flush().await?;

    Ok(create_room::Response::new(room_id).into())
}

#[cfg_attr(
    feature = "conduit_bin",
    get("/_matrix/client/r0/rooms/<_>/event/<_>", data = "<body>")
)]
#[tracing::instrument(skip(db, body))]
pub async fn get_room_event_route(
    db: DatabaseGuard,
    body: Ruma<get_room_event::Request<'_>>,
) -> ConduitResult<get_room_event::Response> {
    let sender_user = body.sender_user.as_ref().expect("user is authenticated");

    if !db.rooms.is_joined(sender_user, &body.room_id)? {
        return Err(Error::BadRequest(
            ErrorKind::Forbidden,
            "You don't have permission to view this room.",
        ));
    }

    Ok(get_room_event::Response {
        event: db
            .rooms
            .get_pdu(&body.event_id)?
            .ok_or(Error::BadRequest(ErrorKind::NotFound, "Event not found."))?
            .to_room_event(),
    }
    .into())
}

#[cfg_attr(
    feature = "conduit_bin",
    post("/_matrix/client/r0/rooms/<_room_id>/upgrade", data = "<body>")
)]
#[tracing::instrument(skip(db, body))]
pub async fn upgrade_room_route(
    db: DatabaseGuard,
    body: Ruma<upgrade_room::Request<'_>>,
    _room_id: String,
) -> ConduitResult<upgrade_room::Response> {
    let sender_user = body.sender_user.as_ref().expect("user is authenticated");

    if !matches!(body.new_version, RoomVersionId::Version6) {
        return Err(Error::BadRequest(
            ErrorKind::UnsupportedRoomVersion,
            "This server does not support that room version.",
        ));
    }

    // Create a replacement room
    let replacement_room = RoomId::new(db.globals.server_name());

    let mutex = Arc::clone(
        db.globals
            .roomid_mutex
            .write()
            .unwrap()
            .entry(body.room_id.clone())
            .or_default(),
    );
    let mutex_lock = mutex.lock().await;

    // Send a m.room.tombstone event to the old room to indicate that it is not intended to be used any further
    // Fail if the sender does not have the required permissions
    let tombstone_event_id = db.rooms.build_and_append_pdu(
        PduBuilder {
            event_type: EventType::RoomTombstone,
            content: serde_json::to_value(ruma::events::room::tombstone::TombstoneEventContent {
                body: "This room has been replaced".to_string(),
                replacement_room: replacement_room.clone(),
            })
            .expect("event is valid, we just created it"),
            unsigned: None,
            state_key: Some("".to_owned()),
            redacts: None,
        },
        sender_user,
        &body.room_id,
        &db,
        &mutex_lock,
    )?;

    // Get the old room federations status
    let federate = serde_json::from_value::<Raw<ruma::events::room::create::CreateEventContent>>(
        db.rooms
            .room_state_get(&body.room_id, &EventType::RoomCreate, "")?
            .ok_or_else(|| Error::bad_database("Found room without m.room.create event."))?
            .content
            .clone(),
    )
    .expect("Raw::from_value always works")
    .deserialize()
    .map_err(|_| Error::bad_database("Invalid room event in database."))?
    .federate;

    // Use the m.room.tombstone event as the predecessor
    let predecessor = Some(ruma::events::room::create::PreviousRoom::new(
        body.room_id.clone(),
        tombstone_event_id,
    ));

    // Send a m.room.create event containing a predecessor field and the applicable room_version
    let mut create_event_content =
        ruma::events::room::create::CreateEventContent::new(sender_user.clone());
    create_event_content.federate = federate;
    create_event_content.room_version = body.new_version.clone();
    create_event_content.predecessor = predecessor;

    db.rooms.build_and_append_pdu(
        PduBuilder {
            event_type: EventType::RoomCreate,
            content: serde_json::to_value(create_event_content)
                .expect("event is valid, we just created it"),
            unsigned: None,
            state_key: Some("".to_owned()),
            redacts: None,
        },
        sender_user,
        &replacement_room,
        &db,
        &mutex_lock,
    )?;

    // Join the new room
    db.rooms.build_and_append_pdu(
        PduBuilder {
            event_type: EventType::RoomMember,
            content: serde_json::to_value(member::MemberEventContent {
                membership: member::MembershipState::Join,
                displayname: db.users.displayname(&sender_user)?,
                avatar_url: db.users.avatar_url(&sender_user)?,
                is_direct: None,
                third_party_invite: None,
                blurhash: db.users.blurhash(&sender_user)?,
            })
            .expect("event is valid, we just created it"),
            unsigned: None,
            state_key: Some(sender_user.to_string()),
            redacts: None,
        },
        sender_user,
        &replacement_room,
        &db,
        &mutex_lock,
    )?;

    // Recommended transferable state events list from the specs
    let transferable_state_events = vec![
        EventType::RoomServerAcl,
        EventType::RoomEncryption,
        EventType::RoomName,
        EventType::RoomAvatar,
        EventType::RoomTopic,
        EventType::RoomGuestAccess,
        EventType::RoomHistoryVisibility,
        EventType::RoomJoinRules,
        EventType::RoomPowerLevels,
    ];

    // Replicate transferable state events to the new room
    for event_type in transferable_state_events {
        let event_content = match db.rooms.room_state_get(&body.room_id, &event_type, "")? {
            Some(v) => v.content.clone(),
            None => continue, // Skipping missing events.
        };

        db.rooms.build_and_append_pdu(
            PduBuilder {
                event_type,
                content: event_content,
                unsigned: None,
                state_key: Some("".to_owned()),
                redacts: None,
            },
            sender_user,
            &replacement_room,
            &db,
            &mutex_lock,
        )?;
    }

    // Moves any local aliases to the new room
    for alias in db.rooms.room_aliases(&body.room_id).filter_map(|r| r.ok()) {
        db.rooms
            .set_alias(&alias, Some(&replacement_room), &db.globals)?;
    }

    // Get the old room power levels
    let mut power_levels_event_content =
        serde_json::from_value::<Raw<ruma::events::room::power_levels::PowerLevelsEventContent>>(
            db.rooms
                .room_state_get(&body.room_id, &EventType::RoomPowerLevels, "")?
                .ok_or_else(|| Error::bad_database("Found room without m.room.create event."))?
                .content
                .clone(),
        )
        .expect("database contains invalid PDU")
        .deserialize()
        .map_err(|_| Error::bad_database("Invalid room event in database."))?;

    // Setting events_default and invite to the greater of 50 and users_default + 1
    let new_level = max(
        50.into(),
        power_levels_event_content.users_default + 1.into(),
    );
    power_levels_event_content.events_default = new_level;
    power_levels_event_content.invite = new_level;

    // Modify the power levels in the old room to prevent sending of events and inviting new users
    let _ = db.rooms.build_and_append_pdu(
        PduBuilder {
            event_type: EventType::RoomPowerLevels,
            content: serde_json::to_value(power_levels_event_content)
                .expect("event is valid, we just created it"),
            unsigned: None,
            state_key: Some("".to_owned()),
            redacts: None,
        },
        sender_user,
        &body.room_id,
        &db,
        &mutex_lock,
    )?;

    drop(mutex_lock);

    db.flush().await?;

    // Return the replacement room id
    Ok(upgrade_room::Response { replacement_room }.into())
}
