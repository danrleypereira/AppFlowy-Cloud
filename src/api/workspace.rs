use access_control::act::Action;
use actix_web::web::{Bytes, Payload};
use actix_web::web::{Data, Json, PayloadConfig};
use actix_web::{web, Scope};
use actix_web::{HttpRequest, Result};
use anyhow::{anyhow, Context};
use bytes::BytesMut;
use collab::entity::EncodedCollab;
use collab_entity::CollabType;
use futures_util::future::try_join_all;
use prost::Message as ProstMessage;
use rayon::prelude::*;
use sqlx::types::uuid;
use std::time::Instant;

use tokio_stream::StreamExt;
use tokio_tungstenite::tungstenite::Message;
use tracing::{error, event, instrument, trace};
use uuid::Uuid;
use validator::Validate;

use app_error::AppError;
use appflowy_collaborate::actix_ws::entities::ClientStreamMessage;
use appflowy_collaborate::indexer::IndexerProvider;
use authentication::jwt::{Authorization, OptionalUserUuid, UserUuid};
use collab_rt_entity::realtime_proto::HttpRealtimeMessage;
use collab_rt_entity::RealtimeMessage;
use collab_rt_protocol::validate_encode_collab;
use database::collab::{CollabStorage, GetCollabOrigin};
use database::user::select_uid_from_email;
use database_entity::dto::PublishCollabItem;
use database_entity::dto::PublishInfo;
use database_entity::dto::*;
use shared_entity::dto::workspace_dto::*;
use shared_entity::response::AppResponseError;
use shared_entity::response::{AppResponse, JsonAppResponse};

use crate::api::util::PayloadReader;
use crate::api::util::{compress_type_from_header_value, device_id_from_headers, CollabValidator};
use crate::api::ws::RealtimeServerAddr;
use crate::biz;
use crate::biz::collab::ops::{
  get_user_favorite_folder_views, get_user_recent_folder_views, get_user_trash_folder_views,
};
use crate::biz::user::user_verify::verify_token;
use crate::biz::workspace;
use crate::biz::workspace::ops::{
  create_comment_on_published_view, create_reaction_on_comment, get_comments_on_published_view,
  get_reactions_on_published_view, remove_comment_on_published_view, remove_reaction_on_comment,
};
use crate::biz::workspace::page_view::{
  create_page, get_page_view_collab, update_page_collab_data,
};
use crate::biz::workspace::publish::get_workspace_default_publish_view_info_meta;
use crate::domain::compression::{
  blocking_decompress, decompress, CompressionType, X_COMPRESSION_TYPE,
};
use crate::state::AppState;

pub const WORKSPACE_ID_PATH: &str = "workspace_id";
pub const COLLAB_OBJECT_ID_PATH: &str = "object_id";

pub const WORKSPACE_PATTERN: &str = "/api/workspace";
pub const WORKSPACE_MEMBER_PATTERN: &str = "/api/workspace/{workspace_id}/member";
pub const WORKSPACE_INVITE_PATTERN: &str = "/api/workspace/{workspace_id}/invite";
pub const COLLAB_PATTERN: &str = "/api/workspace/{workspace_id}/collab/{object_id}";
pub const V1_COLLAB_PATTERN: &str = "/api/workspace/v1/{workspace_id}/collab/{object_id}";
pub const WORKSPACE_PUBLISH_PATTERN: &str = "/api/workspace/{workspace_id}/publish";
pub const WORKSPACE_PUBLISH_NAMESPACE_PATTERN: &str =
  "/api/workspace/{workspace_id}/publish-namespace";

pub fn workspace_scope() -> Scope {
  web::scope("/api/workspace")
    .service(
      web::resource("")
        .route(web::get().to(list_workspace_handler))
        .route(web::post().to(create_workspace_handler))
        .route(web::patch().to(patch_workspace_handler)),
    )
    .service(
      web::resource("/{workspace_id}/invite").route(web::post().to(post_workspace_invite_handler)), // invite members to workspace
    )
    .service(
      web::resource("/invite").route(web::get().to(get_workspace_invite_handler)), // show invites for user
    )
    .service(
      web::resource("/invite/{invite_id}").route(web::get().to(get_workspace_invite_by_id_handler)),
    )
    .service(
      web::resource("/accept-invite/{invite_id}")
        .route(web::post().to(post_accept_workspace_invite_handler)), // accept invitation to workspace
    )
    .service(web::resource("/{workspace_id}").route(web::delete().to(delete_workspace_handler)))
    .service(
      web::resource("/{workspace_id}/settings")
        .route(web::get().to(get_workspace_settings_handler))
        .route(web::post().to(post_workspace_settings_handler)),
    )
    .service(web::resource("/{workspace_id}/open").route(web::put().to(open_workspace_handler)))
    .service(web::resource("/{workspace_id}/leave").route(web::post().to(leave_workspace_handler)))
    .service(
      web::resource("/{workspace_id}/member")
        .route(web::get().to(get_workspace_members_handler))
        .route(web::put().to(update_workspace_member_handler))
        .route(web::delete().to(remove_workspace_member_handler)),
    )
    .service(
      web::resource("/{workspace_id}/member/user/{user_id}")
        .route(web::get().to(get_workspace_member_handler)),
    )
    .service(
      web::resource("/{workspace_id}/collab/{object_id}")
        .app_data(
          PayloadConfig::new(5 * 1024 * 1024), // 5 MB
        )
        .route(web::post().to(create_collab_handler))
        .route(web::get().to(get_collab_handler))
        .route(web::put().to(update_collab_handler))
        .route(web::delete().to(delete_collab_handler)),
    )
    .service(
      web::resource("/v1/{workspace_id}/collab/{object_id}")
        .route(web::get().to(v1_get_collab_handler)),
    )
    .service(
      web::resource("/v1/{workspace_id}/collab/{object_id}/web-update")
        .route(web::post().to(post_web_update_handler)),
    )
    .service(
      web::resource("/{workspace_id}/page-view").route(web::post().to(post_page_view_handler)),
    )
    .service(
      web::resource("/{workspace_id}/page-view/{view_id}")
        .route(web::get().to(get_page_view_handler)),
    )
    .service(
      web::resource("/{workspace_id}/batch/collab")
        .route(web::post().to(batch_create_collab_handler)),
    )
    .service(
      web::resource("/{workspace_id}/usage").route(web::get().to(get_workspace_usage_handler)),
    )
    .service(
      web::resource("/{workspace_id}/{object_id}/snapshot")
        .route(web::get().to(get_collab_snapshot_handler))
        .route(web::post().to(create_collab_snapshot_handler)),
    )
    .service(
      web::resource("/{workspace_id}/{object_id}/snapshot/list")
        .route(web::get().to(get_all_collab_snapshot_list_handler)),
    )
    .service(
      web::resource("/{workspace_id}/collab/{object_id}/member")
        .route(web::post().to(add_collab_member_handler))
        .route(web::get().to(get_collab_member_handler))
        .route(web::put().to(update_collab_member_handler))
        .route(web::delete().to(remove_collab_member_handler)),
    )
    .service(
      web::resource("/published/{publish_namespace}")
        .route(web::get().to(get_default_published_collab_info_meta_handler)),
    )
    .service(
      web::resource("/v1/published/{publish_namespace}/{publish_name}")
        .route(web::get().to(get_v1_published_collab_handler)),
    )
    .service(
      web::resource("/published/{publish_namespace}/{publish_name}/blob")
        .route(web::get().to(get_published_collab_blob_handler)),
    )
    .service(
      web::resource("{workspace_id}/published-duplicate")
        .route(web::post().to(post_published_duplicate_handler)),
    )
    .service(
      web::resource("/{workspace_id}/published-info")
        .route(web::get().to(list_published_collab_info_handler)),
    )
    .service(
      web::resource("/published-info/{view_id}")
        .route(web::get().to(get_published_collab_info_handler)),
    )
    .service(
      web::resource("/published-info/{view_id}/comment")
        .route(web::get().to(get_published_collab_comment_handler))
        .route(web::post().to(post_published_collab_comment_handler))
        .route(web::delete().to(delete_published_collab_comment_handler)),
    )
    .service(
      web::resource("/published-info/{view_id}/reaction")
        .route(web::get().to(get_published_collab_reaction_handler))
        .route(web::post().to(post_published_collab_reaction_handler))
        .route(web::delete().to(delete_published_collab_reaction_handler)),
    )
    .service(
      web::resource("/{workspace_id}/publish-namespace")
        .route(web::put().to(put_publish_namespace_handler))
        .route(web::get().to(get_publish_namespace_handler)),
    )
    .service(
      web::resource("/{workspace_id}/publish-default")
        .route(web::put().to(put_workspace_default_published_view_handler))
        .route(web::delete().to(delete_workspace_default_published_view_handler))
        .route(web::get().to(get_workspace_published_default_info_handler)),
    )
    .service(
      web::resource("/{workspace_id}/publish")
        .route(web::post().to(post_publish_collabs_handler))
        .route(web::delete().to(delete_published_collabs_handler))
        .route(web::patch().to(patch_published_collabs_handler)),
    )
    .service(
      web::resource("/{workspace_id}/folder").route(web::get().to(get_workspace_folder_handler)),
    )
    .service(web::resource("/{workspace_id}/recent").route(web::get().to(get_recent_views_handler)))
    .service(
      web::resource("/{workspace_id}/favorite").route(web::get().to(get_favorite_views_handler)),
    )
    .service(web::resource("/{workspace_id}/trash").route(web::get().to(get_trash_views_handler)))
    .service(
      web::resource("/published-outline/{publish_namespace}")
        .route(web::get().to(get_workspace_publish_outline_handler)),
    )
    .service(
      web::resource("/{workspace_id}/collab/{object_id}/member/list")
        .route(web::get().to(get_collab_member_list_handler)),
    )
    .service(
      web::resource("/{workspace_id}/collab_list")
      .route(web::get().to(batch_get_collab_handler))
      // Web browser can't carry payload when using GET method, so for browser compatibility, we use POST method
      .route(web::post().to(batch_get_collab_handler)),
    )
}

pub fn collab_scope() -> Scope {
  web::scope("/api/realtime").service(
    web::resource("post/stream")
      .app_data(
        PayloadConfig::new(10 * 1024 * 1024), // 10 MB
      )
      .route(web::post().to(post_realtime_message_stream_handler)),
  )
}

// Adds a workspace for user, if success, return the workspace id
#[instrument(skip_all, err)]
async fn create_workspace_handler(
  uuid: UserUuid,
  state: Data<AppState>,
  create_workspace_param: Json<CreateWorkspaceParam>,
) -> Result<Json<AppResponse<AFWorkspace>>> {
  let workspace_name = create_workspace_param
    .into_inner()
    .workspace_name
    .unwrap_or_else(|| format!("workspace_{}", chrono::Utc::now().timestamp()));

  let uid = state.user_cache.get_user_uid(&uuid).await?;
  let new_workspace = workspace::ops::create_workspace_for_user(
    &state.pg_pool,
    state.workspace_access_control.clone(),
    &state.collab_access_control_storage,
    &uuid,
    uid,
    &workspace_name,
  )
  .await?;

  Ok(AppResponse::Ok().with_data(new_workspace).into())
}

// Edit existing workspace
#[instrument(skip_all, err)]
async fn patch_workspace_handler(
  uuid: UserUuid,
  state: Data<AppState>,
  params: Json<PatchWorkspaceParam>,
) -> Result<Json<AppResponse<()>>> {
  let uid = state.user_cache.get_user_uid(&uuid).await?;
  state
    .workspace_access_control
    .enforce_action(&uid, &params.workspace_id.to_string(), Action::Write)
    .await?;
  let params = params.into_inner();
  workspace::ops::patch_workspace(
    &state.pg_pool,
    &params.workspace_id,
    params.workspace_name.as_deref(),
    params.workspace_icon.as_deref(),
  )
  .await?;
  Ok(AppResponse::Ok().into())
}

async fn delete_workspace_handler(
  user_uuid: UserUuid,
  workspace_id: web::Path<Uuid>,
  state: Data<AppState>,
) -> Result<Json<AppResponse<()>>> {
  let uid = state.user_cache.get_user_uid(&user_uuid).await?;
  state
    .workspace_access_control
    .enforce_action(&uid, &workspace_id.to_string(), Action::Delete)
    .await?;
  workspace::ops::delete_workspace_for_user(
    state.pg_pool.clone(),
    *workspace_id,
    state.bucket_storage.clone(),
  )
  .await?;
  Ok(AppResponse::Ok().into())
}

/// Get all user owned and shared workspaces
#[instrument(skip_all, err)]
async fn list_workspace_handler(
  uuid: UserUuid,
  state: Data<AppState>,
  query: web::Query<QueryWorkspaceParam>,
) -> Result<JsonAppResponse<Vec<AFWorkspace>>> {
  let workspaces = workspace::ops::get_all_user_workspaces(
    &state.pg_pool,
    &uuid,
    query.into_inner().include_member_count.unwrap_or(false),
  )
  .await?;
  Ok(AppResponse::Ok().with_data(workspaces).into())
}

#[instrument(skip(payload, state), err)]
async fn post_workspace_invite_handler(
  user_uuid: UserUuid,
  workspace_id: web::Path<Uuid>,
  payload: Json<Vec<WorkspaceMemberInvitation>>,
  state: Data<AppState>,
) -> Result<JsonAppResponse<()>> {
  let uid = state.user_cache.get_user_uid(&user_uuid).await?;
  state
    .workspace_access_control
    .enforce_role(&uid, &workspace_id.to_string(), AFRole::Owner)
    .await?;

  let invited_members = payload.into_inner();
  workspace::ops::invite_workspace_members(
    &state.mailer,
    &state.gotrue_admin,
    &state.pg_pool,
    &state.gotrue_client,
    &user_uuid,
    &workspace_id,
    invited_members,
    state.config.appflowy_web_url.as_deref(),
  )
  .await?;
  Ok(AppResponse::Ok().into())
}

async fn get_workspace_invite_handler(
  user_uuid: UserUuid,
  state: Data<AppState>,
  query: web::Query<WorkspaceInviteQuery>,
) -> Result<JsonAppResponse<Vec<AFWorkspaceInvitation>>> {
  let query = query.into_inner();
  let res =
    workspace::ops::list_workspace_invitations_for_user(&state.pg_pool, &user_uuid, query.status)
      .await?;
  Ok(AppResponse::Ok().with_data(res).into())
}

async fn get_workspace_invite_by_id_handler(
  user_uuid: UserUuid,
  state: Data<AppState>,
  invite_id: web::Path<Uuid>,
) -> Result<JsonAppResponse<AFWorkspaceInvitation>> {
  let invite_id = invite_id.into_inner();
  let res =
    workspace::ops::get_workspace_invitations_for_user(&state.pg_pool, &user_uuid, &invite_id)
      .await?;
  Ok(AppResponse::Ok().with_data(res).into())
}

async fn post_accept_workspace_invite_handler(
  auth: Authorization,
  invite_id: web::Path<Uuid>,
  state: Data<AppState>,
) -> Result<JsonAppResponse<()>> {
  let _is_new = verify_token(&auth.token, state.as_ref()).await?;
  let user_uuid = auth.uuid()?;
  let user_uid = state.user_cache.get_user_uid(&user_uuid).await?;
  let invite_id = invite_id.into_inner();
  workspace::ops::accept_workspace_invite(
    &state.pg_pool,
    state.workspace_access_control.clone(),
    user_uid,
    &user_uuid,
    &invite_id,
  )
  .await?;
  Ok(AppResponse::Ok().into())
}

#[instrument(skip_all, err, fields(user_uuid))]
async fn get_workspace_settings_handler(
  user_uuid: UserUuid,
  state: Data<AppState>,
  workspace_id: web::Path<Uuid>,
) -> Result<JsonAppResponse<AFWorkspaceSettings>> {
  let uid = state.user_cache.get_user_uid(&user_uuid).await?;
  state
    .workspace_access_control
    .enforce_action(&uid, &workspace_id.to_string(), Action::Read)
    .await?;
  let settings = workspace::ops::get_workspace_settings(&state.pg_pool, &workspace_id).await?;
  Ok(AppResponse::Ok().with_data(settings).into())
}

#[instrument(level = "info", skip_all, err, fields(user_uuid))]
async fn post_workspace_settings_handler(
  user_uuid: UserUuid,
  state: Data<AppState>,
  workspace_id: web::Path<Uuid>,
  data: Json<AFWorkspaceSettingsChange>,
) -> Result<JsonAppResponse<AFWorkspaceSettings>> {
  let data = data.into_inner();
  trace!("workspace settings: {:?}", data);
  let uid = state.user_cache.get_user_uid(&user_uuid).await?;
  state
    .workspace_access_control
    .enforce_action(&uid, &workspace_id.to_string(), Action::Write)
    .await?;
  let settings =
    workspace::ops::update_workspace_settings(&state.pg_pool, &workspace_id, data).await?;
  Ok(AppResponse::Ok().with_data(settings).into())
}

#[instrument(skip_all, err)]
async fn get_workspace_members_handler(
  user_uuid: UserUuid,
  state: Data<AppState>,
  workspace_id: web::Path<Uuid>,
) -> Result<JsonAppResponse<Vec<AFWorkspaceMember>>> {
  let uid = state.user_cache.get_user_uid(&user_uuid).await?;
  state
    .workspace_access_control
    .enforce_role(&uid, &workspace_id.to_string(), AFRole::Member)
    .await?;
  let members = workspace::ops::get_workspace_members(&state.pg_pool, &workspace_id)
    .await?
    .into_iter()
    .map(|member| AFWorkspaceMember {
      name: member.name,
      email: member.email,
      role: member.role,
      avatar_url: None,
    })
    .collect();

  Ok(AppResponse::Ok().with_data(members).into())
}

#[instrument(skip_all, err)]
async fn remove_workspace_member_handler(
  user_uuid: UserUuid,
  payload: Json<WorkspaceMembers>,
  state: Data<AppState>,
  workspace_id: web::Path<Uuid>,
) -> Result<JsonAppResponse<()>> {
  let uid = state.user_cache.get_user_uid(&user_uuid).await?;
  state
    .workspace_access_control
    .enforce_role(&uid, &workspace_id.to_string(), AFRole::Owner)
    .await?;

  let member_emails = payload
    .into_inner()
    .0
    .into_iter()
    .map(|member| member.0)
    .collect::<Vec<String>>();
  workspace::ops::remove_workspace_members(
    &state.pg_pool,
    &workspace_id,
    &member_emails,
    state.workspace_access_control.clone(),
  )
  .await?;

  Ok(AppResponse::Ok().into())
}

#[instrument(skip_all, err)]
async fn get_workspace_member_handler(
  user_uuid: UserUuid,
  state: Data<AppState>,
  path: web::Path<(Uuid, i64)>,
) -> Result<JsonAppResponse<AFWorkspaceMember>> {
  let (workspace_id, user_uuid_to_retrieved) = path.into_inner();
  let uid = state.user_cache.get_user_uid(&user_uuid).await?;
  // Guest users can not get workspace members
  state
    .workspace_access_control
    .enforce_role(&uid, &workspace_id.to_string(), AFRole::Member)
    .await?;
  let member_row =
    workspace::ops::get_workspace_member(&user_uuid_to_retrieved, &state.pg_pool, &workspace_id)
      .await?;
  let member = AFWorkspaceMember {
    name: member_row.name,
    email: member_row.email,
    role: member_row.role,
    avatar_url: None,
  };

  Ok(AppResponse::Ok().with_data(member).into())
}

#[instrument(level = "debug", skip_all, err)]
async fn open_workspace_handler(
  user_uuid: UserUuid,
  state: Data<AppState>,
  workspace_id: web::Path<Uuid>,
) -> Result<JsonAppResponse<AFWorkspace>> {
  let workspace_id = workspace_id.into_inner();
  let uid = state.user_cache.get_user_uid(&user_uuid).await?;
  state
    .workspace_access_control
    .enforce_action(&uid, &workspace_id.to_string(), Action::Read)
    .await?;
  let workspace = workspace::ops::open_workspace(&state.pg_pool, &user_uuid, &workspace_id).await?;
  Ok(AppResponse::Ok().with_data(workspace).into())
}

#[instrument(level = "debug", skip_all, err)]
async fn leave_workspace_handler(
  user_uuid: UserUuid,
  state: Data<AppState>,
  workspace_id: web::Path<Uuid>,
) -> Result<JsonAppResponse<()>> {
  let workspace_id = workspace_id.into_inner();
  workspace::ops::leave_workspace(
    &state.pg_pool,
    &workspace_id,
    &user_uuid,
    state.workspace_access_control.clone(),
  )
  .await?;
  Ok(AppResponse::Ok().into())
}

#[instrument(level = "debug", skip_all, err)]
async fn update_workspace_member_handler(
  user_uuid: UserUuid,
  payload: Json<WorkspaceMemberChangeset>,
  state: Data<AppState>,
  workspace_id: web::Path<Uuid>,
) -> Result<JsonAppResponse<()>> {
  let workspace_id = workspace_id.into_inner();
  let uid = state.user_cache.get_user_uid(&user_uuid).await?;
  state
    .workspace_access_control
    .enforce_role(&uid, &workspace_id.to_string(), AFRole::Owner)
    .await?;

  let changeset = payload.into_inner();

  if changeset.role.is_some() {
    let changeset_uid = select_uid_from_email(&state.pg_pool, &changeset.email)
      .await
      .map_err(AppResponseError::from)?;
    workspace::ops::update_workspace_member(
      &changeset_uid,
      &state.pg_pool,
      &workspace_id,
      &changeset,
      state.workspace_access_control.clone(),
    )
    .await?;
  }

  Ok(AppResponse::Ok().into())
}

#[instrument(skip(state, payload), err)]
async fn create_collab_handler(
  user_uuid: UserUuid,
  payload: Bytes,
  state: Data<AppState>,
  req: HttpRequest,
) -> Result<Json<AppResponse<()>>> {
  let uid = state.user_cache.get_user_uid(&user_uuid).await?;
  let params = match req.headers().get(X_COMPRESSION_TYPE) {
    None => serde_json::from_slice::<CreateCollabParams>(&payload).map_err(|err| {
      AppError::InvalidRequest(format!(
        "Failed to parse CreateCollabParams from JSON: {}",
        err
      ))
    })?,
    Some(_) => match compress_type_from_header_value(req.headers())? {
      CompressionType::Brotli { buffer_size } => {
        let decompress_data = blocking_decompress(payload.to_vec(), buffer_size).await?;
        CreateCollabParams::from_bytes(&decompress_data).map_err(|err| {
          AppError::InvalidRequest(format!(
            "Failed to parse CreateCollabParams with brotli decompression data: {}",
            err
          ))
        })?
      },
    },
  };

  let (mut params, workspace_id) = params.split();

  if params.object_id == workspace_id {
    // Only the object with [CollabType::Folder] can have the same object_id as workspace_id. But
    // it should use create workspace API
    return Err(
      AppError::InvalidRequest("object_id cannot be the same as workspace_id".to_string()).into(),
    );
  }

  if let Err(err) = params.check_encode_collab().await {
    return Err(
      AppError::NoRequiredData(format!(
        "collab doc state is not correct:{},{}",
        params.object_id, err
      ))
      .into(),
    );
  }

  if state
    .indexer_provider
    .can_index_workspace(&workspace_id)
    .await?
  {
    match state
      .indexer_provider
      .create_collab_embeddings(&params)
      .await
    {
      Ok(embeddings) => params.embeddings = embeddings,
      Err(err) => tracing::warn!(
        "failed to fetch embeddings for document {}: {}",
        params.object_id,
        err
      ),
    }
  }

  let mut transaction = state
    .pg_pool
    .begin()
    .await
    .context("acquire transaction to upsert collab")
    .map_err(AppError::from)?;

  let action = format!("Create new collab: {}", params);
  state
    .collab_access_control_storage
    .insert_new_collab_with_transaction(&workspace_id, &uid, params, &mut transaction, &action)
    .await?;

  transaction
    .commit()
    .await
    .context("fail to commit the transaction to upsert collab")
    .map_err(AppError::from)?;

  Ok(Json(AppResponse::Ok()))
}

#[instrument(skip(state, payload), err)]
async fn batch_create_collab_handler(
  user_uuid: UserUuid,
  workspace_id: web::Path<Uuid>,
  mut payload: Payload,
  state: Data<AppState>,
  req: HttpRequest,
) -> Result<Json<AppResponse<()>>> {
  let uid = state.user_cache.get_user_uid(&user_uuid).await?;
  let workspace_id = workspace_id.into_inner().to_string();
  let compress_type = compress_type_from_header_value(req.headers())?;
  event!(tracing::Level::DEBUG, "start decompressing collab list");

  let mut payload_buffer = Vec::new();
  let mut offset_len_list = Vec::new();
  let mut current_offset = 0;
  let start = Instant::now();
  while let Some(item) = payload.next().await {
    if let Ok(bytes) = item {
      payload_buffer.extend_from_slice(&bytes);
      while current_offset + 4 <= payload_buffer.len() {
        // The length of the next frame is determined by the first 4 bytes
        let size = u32::from_be_bytes([
          payload_buffer[current_offset],
          payload_buffer[current_offset + 1],
          payload_buffer[current_offset + 2],
          payload_buffer[current_offset + 3],
        ]) as usize;

        // Ensure there is enough data for the frame (4 bytes for size + `size` bytes for data)
        if current_offset + 4 + size > payload_buffer.len() {
          break;
        }

        // Collect the (offset, len) for the current frame (data starts at current_offset + 4)
        offset_len_list.push((current_offset + 4, size));
        current_offset += 4 + size;
      }
    }
  }
  // Perform decompression and processing in a Rayon thread pool
  let mut collab_params_list = tokio::task::spawn_blocking(move || match compress_type {
    CompressionType::Brotli { buffer_size } => offset_len_list
      .into_par_iter()
      .filter_map(|(offset, len)| {
        let compressed_data = &payload_buffer[offset..offset + len];
        match decompress(compressed_data.to_vec(), buffer_size) {
          Ok(decompressed_data) => {
            let params = CollabParams::from_bytes(&decompressed_data).ok()?;
            if params.validate().is_ok() {
              match validate_encode_collab(
                &params.object_id,
                &params.encoded_collab_v1,
                &params.collab_type,
              ) {
                Ok(_) => Some(params),
                Err(_) => None,
              }
            } else {
              None
            }
          },
          Err(err) => {
            error!("Failed to decompress data: {:?}", err);
            None
          },
        }
      })
      .collect::<Vec<_>>(),
  })
  .await
  .map_err(|_| AppError::InvalidRequest("Failed to decompress data".to_string()))?;

  if collab_params_list.is_empty() {
    return Err(AppError::InvalidRequest("Empty collab params list".to_string()).into());
  }

  let total_size = collab_params_list
    .iter()
    .fold(0, |acc, x| acc + x.encoded_collab_v1.len());
  event!(
    tracing::Level::INFO,
    "decompressed {} collab objects in {:?}",
    collab_params_list.len(),
    start.elapsed()
  );

  if state
    .indexer_provider
    .can_index_workspace(&workspace_id)
    .await?
  {
    if let Err(err) = fetch_embeddings(&state.indexer_provider, &mut collab_params_list).await {
      tracing::warn!(
        "failed to fetch embeddings for {} new documents: {}",
        collab_params_list.len(),
        err
      );
    }
  }

  let start = Instant::now();
  state
    .collab_access_control_storage
    .batch_insert_new_collab(&workspace_id, &uid, collab_params_list)
    .await?;

  event!(
    tracing::Level::INFO,
    "inserted collab objects to disk in {:?}, total size:{}",
    start.elapsed(),
    total_size
  );

  Ok(Json(AppResponse::Ok()))
}

// Deprecated
async fn get_collab_handler(
  user_uuid: UserUuid,
  payload: Json<QueryCollabParams>,
  state: Data<AppState>,
) -> Result<Json<AppResponse<CollabResponse>>> {
  let uid = state
    .user_cache
    .get_user_uid(&user_uuid)
    .await
    .map_err(AppResponseError::from)?;
  let params = payload.into_inner();
  let object_id = params.object_id.clone();
  let encode_collab = state
    .collab_access_control_storage
    .get_encode_collab(GetCollabOrigin::User { uid }, params, true)
    .await
    .map_err(AppResponseError::from)?;

  let resp = CollabResponse {
    encode_collab,
    object_id,
  };

  Ok(Json(AppResponse::Ok().with_data(resp)))
}

async fn v1_get_collab_handler(
  user_uuid: UserUuid,
  path: web::Path<(String, String)>,
  query: web::Query<CollabTypeParam>,
  state: Data<AppState>,
) -> Result<Json<AppResponse<CollabResponse>>> {
  let (workspace_id, object_id) = path.into_inner();
  let collab_type = query.into_inner().collab_type;
  let uid = state
    .user_cache
    .get_user_uid(&user_uuid)
    .await
    .map_err(AppResponseError::from)?;

  let param = QueryCollabParams {
    workspace_id,
    inner: QueryCollab {
      object_id: object_id.clone(),
      collab_type,
    },
  };

  let encode_collab = state
    .collab_access_control_storage
    .get_encode_collab(GetCollabOrigin::User { uid }, param, true)
    .await
    .map_err(AppResponseError::from)?;

  let resp = CollabResponse {
    encode_collab,
    object_id,
  };

  Ok(Json(AppResponse::Ok().with_data(resp)))
}

async fn post_web_update_handler(
  user_uuid: UserUuid,
  path: web::Path<(Uuid, Uuid)>,
  payload: Json<UpdateCollabWebParams>,
  state: Data<AppState>,
) -> Result<Json<AppResponse<()>>> {
  let (workspace_id, object_id) = path.into_inner();
  let collab_type = payload.collab_type.clone();
  let uid = state
    .user_cache
    .get_user_uid(&user_uuid)
    .await
    .map_err(AppResponseError::from)?;
  update_page_collab_data(
    state.collab_access_control_storage.clone(),
    state.metrics.appflowy_web_metrics.clone(),
    uid,
    workspace_id,
    object_id,
    collab_type,
    &payload.doc_state,
  )
  .await?;
  Ok(Json(AppResponse::Ok()))
}

async fn post_page_view_handler(
  user_uuid: UserUuid,
  path: web::Path<Uuid>,
  payload: Json<CreatePageParams>,
  state: Data<AppState>,
) -> Result<Json<AppResponse<Page>>> {
  let uid = state.user_cache.get_user_uid(&user_uuid).await?;
  let workspace_uuid = path.into_inner();
  let page = create_page(
    &state.pg_pool,
    &state.collab_access_control_storage,
    uid,
    workspace_uuid,
    &payload.parent_view_id,
    &payload.layout,
  )
  .await?;
  Ok(Json(AppResponse::Ok().with_data(page)))
}

async fn get_page_view_handler(
  user_uuid: UserUuid,
  path: web::Path<(Uuid, String)>,
  state: Data<AppState>,
) -> Result<Json<AppResponse<PageCollab>>> {
  let (workspace_uuid, view_id) = path.into_inner();
  let uid = state
    .user_cache
    .get_user_uid(&user_uuid)
    .await
    .map_err(AppResponseError::from)?;

  let page_collab = get_page_view_collab(
    &state.pg_pool,
    &state.collab_access_control_storage,
    uid,
    workspace_uuid,
    &view_id,
  )
  .await?;
  Ok(Json(AppResponse::Ok().with_data(page_collab)))
}

#[instrument(level = "trace", skip_all, err)]
async fn get_collab_snapshot_handler(
  payload: Json<QuerySnapshotParams>,
  path: web::Path<(String, String)>,
  state: Data<AppState>,
) -> Result<Json<AppResponse<SnapshotData>>> {
  let (workspace_id, object_id) = path.into_inner();
  let data = state
    .collab_access_control_storage
    .get_collab_snapshot(&workspace_id.to_string(), &object_id, &payload.snapshot_id)
    .await
    .map_err(AppResponseError::from)?;

  Ok(Json(AppResponse::Ok().with_data(data)))
}

#[instrument(level = "trace", skip_all, err)]
async fn create_collab_snapshot_handler(
  user_uuid: UserUuid,
  state: Data<AppState>,
  path: web::Path<(String, String)>,
  payload: Json<CollabType>,
) -> Result<Json<AppResponse<AFSnapshotMeta>>> {
  let (workspace_id, object_id) = path.into_inner();
  let collab_type = payload.into_inner();
  let uid = state
    .user_cache
    .get_user_uid(&user_uuid)
    .await
    .map_err(AppResponseError::from)?;
  let encoded_collab_v1 = state
    .collab_access_control_storage
    .get_encode_collab(
      GetCollabOrigin::User { uid },
      QueryCollabParams::new(&object_id, collab_type.clone(), &workspace_id),
      true,
    )
    .await?
    .encode_to_bytes()
    .unwrap();

  let meta = state
    .collab_access_control_storage
    .create_snapshot(InsertSnapshotParams {
      object_id,
      workspace_id,
      encoded_collab_v1,
      collab_type,
    })
    .await?;

  Ok(Json(AppResponse::Ok().with_data(meta)))
}

#[instrument(level = "trace", skip(path, state), err)]
async fn get_all_collab_snapshot_list_handler(
  _user_uuid: UserUuid,
  path: web::Path<(String, String)>,
  state: Data<AppState>,
) -> Result<Json<AppResponse<AFSnapshotMetas>>> {
  let (_, object_id) = path.into_inner();
  let data = state
    .collab_access_control_storage
    .get_collab_snapshot_list(&object_id)
    .await
    .map_err(AppResponseError::from)?;
  Ok(Json(AppResponse::Ok().with_data(data)))
}

#[instrument(level = "debug", skip(payload, state), err)]
async fn batch_get_collab_handler(
  user_uuid: UserUuid,
  state: Data<AppState>,
  payload: Json<BatchQueryCollabParams>,
) -> Result<Json<AppResponse<BatchQueryCollabResult>>> {
  let uid = state
    .user_cache
    .get_user_uid(&user_uuid)
    .await
    .map_err(AppResponseError::from)?;
  let result = BatchQueryCollabResult(
    state
      .collab_access_control_storage
      .batch_get_collab(&uid, payload.into_inner().0, false)
      .await,
  );
  Ok(Json(AppResponse::Ok().with_data(result)))
}

#[instrument(skip(state, payload), err)]
async fn update_collab_handler(
  user_uuid: UserUuid,
  payload: Json<CreateCollabParams>,
  state: Data<AppState>,
) -> Result<Json<AppResponse<()>>> {
  let (params, workspace_id) = payload.into_inner().split();
  let uid = state.user_cache.get_user_uid(&user_uuid).await?;

  let create_params = CreateCollabParams::from((workspace_id.to_string(), params));
  let (mut params, workspace_id) = create_params.split();
  if let Some(indexer) = state
    .indexer_provider
    .indexer_for(params.collab_type.clone())
  {
    if state
      .indexer_provider
      .can_index_workspace(&workspace_id)
      .await?
    {
      let (encoded, mut mut_params) = tokio::task::spawn_blocking(move || {
        EncodedCollab::decode_from_bytes(&params.encoded_collab_v1)
          .map(|encoded_collab| (encoded_collab, params))
          .map_err(|err| AppError::InvalidRequest(format!("Failed to decode collab `{}", err)))
      })
      .await
      .map_err(|err| AppError::Internal(err.into()))??;

      match indexer.index(&mut_params.object_id, encoded).await {
        Ok(embeddings) => mut_params.embeddings = embeddings,
        Err(err) => tracing::warn!(
          "failed to fetch embeddings for document {}: {}",
          mut_params.object_id,
          err
        ),
      }

      params = mut_params;
    }
  }

  state
    .collab_access_control_storage
    .queue_insert_or_update_collab(&workspace_id, &uid, params, false)
    .await?;
  Ok(AppResponse::Ok().into())
}

#[instrument(level = "info", skip(state, payload), err)]
async fn delete_collab_handler(
  user_uuid: UserUuid,
  payload: Json<DeleteCollabParams>,
  state: Data<AppState>,
) -> Result<Json<AppResponse<()>>> {
  let payload = payload.into_inner();
  payload.validate().map_err(AppError::from)?;

  let uid = state
    .user_cache
    .get_user_uid(&user_uuid)
    .await
    .map_err(AppResponseError::from)?;

  state
    .collab_access_control_storage
    .delete_collab(&payload.workspace_id, &uid, &payload.object_id)
    .await
    .map_err(AppResponseError::from)?;

  Ok(AppResponse::Ok().into())
}

#[instrument(level = "debug", skip(state, payload), err)]
async fn add_collab_member_handler(
  payload: Json<InsertCollabMemberParams>,
  state: Data<AppState>,
) -> Result<Json<AppResponse<()>>> {
  let payload = payload.into_inner();
  if !state.collab_cache.is_exist(&payload.object_id).await? {
    return Err(
      AppError::RecordNotFound(format!(
        "Fail to insert collab member. The Collab with object_id {} does not exist",
        payload.object_id
      ))
      .into(),
    );
  }

  biz::collab::ops::create_collab_member(
    &state.pg_pool,
    &payload,
    state.collab_access_control.clone(),
  )
  .await?;
  Ok(Json(AppResponse::Ok()))
}

#[instrument(level = "debug", skip(state, payload), err)]
async fn update_collab_member_handler(
  user_uuid: UserUuid,
  payload: Json<UpdateCollabMemberParams>,
  state: Data<AppState>,
) -> Result<Json<AppResponse<()>>> {
  let payload = payload.into_inner();

  if !state.collab_cache.is_exist(&payload.object_id).await? {
    return Err(
      AppError::RecordNotFound(format!(
        "Fail to update collab member. The Collab with object_id {} does not exist",
        payload.object_id
      ))
      .into(),
    );
  }
  biz::collab::ops::upsert_collab_member(
    &state.pg_pool,
    &user_uuid,
    &payload,
    state.collab_access_control.clone(),
  )
  .await?;
  Ok(Json(AppResponse::Ok()))
}
#[instrument(level = "debug", skip(state, payload), err)]
async fn get_collab_member_handler(
  payload: Json<CollabMemberIdentify>,
  state: Data<AppState>,
) -> Result<Json<AppResponse<AFCollabMember>>> {
  let payload = payload.into_inner();
  let member = biz::collab::ops::get_collab_member(&state.pg_pool, &payload).await?;
  Ok(Json(AppResponse::Ok().with_data(member)))
}

#[instrument(skip(state, payload), err)]
async fn remove_collab_member_handler(
  payload: Json<CollabMemberIdentify>,
  state: Data<AppState>,
) -> Result<Json<AppResponse<()>>> {
  let payload = payload.into_inner();
  biz::collab::ops::delete_collab_member(
    &state.pg_pool,
    &payload,
    state.collab_access_control.clone(),
  )
  .await?;

  Ok(Json(AppResponse::Ok()))
}

async fn put_workspace_default_published_view_handler(
  user_uuid: UserUuid,
  workspace_id: web::Path<Uuid>,
  payload: Json<UpdateDefaultPublishView>,
  state: Data<AppState>,
) -> Result<Json<AppResponse<()>>> {
  let uid = state.user_cache.get_user_uid(&user_uuid).await?;
  state
    .workspace_access_control
    .enforce_role(&uid, &workspace_id.to_string(), AFRole::Owner)
    .await?;
  let new_default_pub_view_id = payload.into_inner().view_id;
  biz::workspace::publish::set_workspace_default_publish_view(
    &state.pg_pool,
    &workspace_id,
    &new_default_pub_view_id,
  )
  .await?;
  Ok(Json(AppResponse::Ok()))
}

async fn delete_workspace_default_published_view_handler(
  user_uuid: UserUuid,
  workspace_id: web::Path<Uuid>,
  state: Data<AppState>,
) -> Result<Json<AppResponse<()>>> {
  let uid = state.user_cache.get_user_uid(&user_uuid).await?;
  state
    .workspace_access_control
    .enforce_role(&uid, &workspace_id.to_string(), AFRole::Owner)
    .await?;
  biz::workspace::publish::unset_workspace_default_publish_view(&state.pg_pool, &workspace_id)
    .await?;
  Ok(Json(AppResponse::Ok()))
}

async fn get_workspace_published_default_info_handler(
  workspace_id: web::Path<Uuid>,
  state: Data<AppState>,
) -> Result<Json<AppResponse<PublishInfo>>> {
  let workspace_id = workspace_id.into_inner();
  let info =
    biz::workspace::publish::get_workspace_default_publish_view_info(&state.pg_pool, &workspace_id)
      .await?;
  Ok(Json(AppResponse::Ok().with_data(info)))
}

async fn put_publish_namespace_handler(
  user_uuid: UserUuid,
  workspace_id: web::Path<Uuid>,
  payload: Json<UpdatePublishNamespace>,
  state: Data<AppState>,
) -> Result<Json<AppResponse<()>>> {
  let workspace_id = workspace_id.into_inner();
  let uid = state.user_cache.get_user_uid(&user_uuid).await?;
  state
    .workspace_access_control
    .enforce_role(&uid, &workspace_id.to_string(), AFRole::Owner)
    .await?;
  let UpdatePublishNamespace {
    old_namespace,
    new_namespace,
  } = payload.into_inner();
  biz::workspace::publish::set_workspace_namespace(
    &state.pg_pool,
    &workspace_id,
    &old_namespace,
    &new_namespace,
  )
  .await?;
  Ok(Json(AppResponse::Ok()))
}

async fn get_publish_namespace_handler(
  workspace_id: web::Path<Uuid>,
  state: Data<AppState>,
) -> Result<Json<AppResponse<String>>> {
  let workspace_id = workspace_id.into_inner();
  let namespace =
    biz::workspace::publish::get_workspace_publish_namespace(&state.pg_pool, &workspace_id).await?;
  Ok(Json(AppResponse::Ok().with_data(namespace)))
}

async fn get_default_published_collab_info_meta_handler(
  publish_namespace: web::Path<String>,
  state: Data<AppState>,
) -> Result<Json<AppResponse<PublishInfoMeta<serde_json::Value>>>> {
  let publish_namespace = publish_namespace.into_inner();
  let (info, meta) =
    get_workspace_default_publish_view_info_meta(&state.pg_pool, &publish_namespace).await?;
  Ok(Json(
    AppResponse::Ok().with_data(PublishInfoMeta { info, meta }),
  ))
}

async fn get_v1_published_collab_handler(
  path_param: web::Path<(String, String)>,
  state: Data<AppState>,
) -> Result<Json<AppResponse<serde_json::Value>>> {
  let (workspace_namespace, publish_name) = path_param.into_inner();
  let metadata = state
    .published_collab_store
    .get_collab_metadata(&workspace_namespace, &publish_name)
    .await?;
  Ok(Json(AppResponse::Ok().with_data(metadata)))
}

async fn get_published_collab_blob_handler(
  path_param: web::Path<(String, String)>,
  state: Data<AppState>,
) -> Result<Vec<u8>> {
  let (publish_namespace, publish_name) = path_param.into_inner();
  let collab_data = state
    .published_collab_store
    .get_collab_blob_by_publish_namespace(&publish_namespace, &publish_name)
    .await?;
  Ok(collab_data)
}

async fn post_published_duplicate_handler(
  user_uuid: UserUuid,
  workspace_id: web::Path<String>,
  state: Data<AppState>,
  params: Json<PublishedDuplicate>,
) -> Result<Json<AppResponse<()>>> {
  let uid = state.user_cache.get_user_uid(&user_uuid).await?;
  state
    .workspace_access_control
    .enforce_action(&uid, &workspace_id.to_string(), Action::Write)
    .await?;
  let params = params.into_inner();
  biz::workspace::publish_dup::duplicate_published_collab_to_workspace(
    &state.pg_pool,
    state.bucket_client.clone(),
    state.collab_access_control_storage.clone(),
    uid,
    params.published_view_id,
    workspace_id.into_inner(),
    params.dest_view_id,
  )
  .await?;
  Ok(Json(AppResponse::Ok()))
}

async fn list_published_collab_info_handler(
  workspace_id: web::Path<Uuid>,
  state: Data<AppState>,
) -> Result<Json<AppResponse<Vec<PublishInfoView>>>> {
  let publish_infos = biz::workspace::publish::list_collab_publish_info(
    state.published_collab_store.as_ref(),
    &state.collab_access_control_storage,
    &workspace_id.into_inner(),
  )
  .await?;

  Ok(Json(AppResponse::Ok().with_data(publish_infos)))
}

async fn get_published_collab_info_handler(
  view_id: web::Path<Uuid>,
  state: Data<AppState>,
) -> Result<Json<AppResponse<PublishInfo>>> {
  let view_id = view_id.into_inner();
  let collab_data = state
    .published_collab_store
    .get_collab_publish_info(&view_id)
    .await?;
  Ok(Json(AppResponse::Ok().with_data(collab_data)))
}

async fn get_published_collab_comment_handler(
  view_id: web::Path<Uuid>,
  optional_user_uuid: OptionalUserUuid,
  state: Data<AppState>,
) -> Result<JsonAppResponse<GlobalComments>> {
  let view_id = view_id.into_inner();
  let comments =
    get_comments_on_published_view(&state.pg_pool, &view_id, &optional_user_uuid).await?;
  let resp = GlobalComments { comments };
  Ok(Json(AppResponse::Ok().with_data(resp)))
}

async fn post_published_collab_comment_handler(
  user_uuid: UserUuid,
  view_id: web::Path<Uuid>,
  state: Data<AppState>,
  data: Json<CreateGlobalCommentParams>,
) -> Result<JsonAppResponse<()>> {
  let view_id = view_id.into_inner();
  create_comment_on_published_view(
    &state.pg_pool,
    &view_id,
    &data.reply_comment_id,
    &data.content,
    &user_uuid,
  )
  .await?;
  Ok(Json(AppResponse::Ok()))
}

async fn delete_published_collab_comment_handler(
  user_uuid: UserUuid,
  view_id: web::Path<Uuid>,
  state: Data<AppState>,
  data: Json<DeleteGlobalCommentParams>,
) -> Result<JsonAppResponse<()>> {
  let view_id = view_id.into_inner();
  remove_comment_on_published_view(&state.pg_pool, &view_id, &data.comment_id, &user_uuid).await?;
  Ok(Json(AppResponse::Ok()))
}

async fn get_published_collab_reaction_handler(
  view_id: web::Path<Uuid>,
  query: web::Query<GetReactionQueryParams>,
  state: Data<AppState>,
) -> Result<JsonAppResponse<Reactions>> {
  let view_id = view_id.into_inner();
  let reactions =
    get_reactions_on_published_view(&state.pg_pool, &view_id, &query.comment_id).await?;
  let resp = Reactions { reactions };
  Ok(Json(AppResponse::Ok().with_data(resp)))
}

async fn post_published_collab_reaction_handler(
  user_uuid: UserUuid,
  view_id: web::Path<Uuid>,
  data: Json<CreateReactionParams>,
  state: Data<AppState>,
) -> Result<JsonAppResponse<()>> {
  let view_id = view_id.into_inner();
  create_reaction_on_comment(
    &state.pg_pool,
    &data.comment_id,
    &view_id,
    &data.reaction_type,
    &user_uuid,
  )
  .await?;
  Ok(Json(AppResponse::Ok()))
}

async fn delete_published_collab_reaction_handler(
  user_uuid: UserUuid,
  data: Json<DeleteReactionParams>,
  state: Data<AppState>,
) -> Result<JsonAppResponse<()>> {
  remove_reaction_on_comment(
    &state.pg_pool,
    &data.comment_id,
    &data.reaction_type,
    &user_uuid,
  )
  .await?;
  Ok(Json(AppResponse::Ok()))
}

async fn post_publish_collabs_handler(
  workspace_id: web::Path<Uuid>,
  user_uuid: UserUuid,
  payload: Payload,
  state: Data<AppState>,
) -> Result<Json<AppResponse<()>>> {
  let workspace_id = workspace_id.into_inner();

  let mut accumulator = Vec::<PublishCollabItem<serde_json::Value, Vec<u8>>>::new();
  let mut payload_reader: PayloadReader = PayloadReader::new(payload);

  loop {
    let meta: PublishCollabMetadata<serde_json::Value> = {
      let meta_len = payload_reader.read_u32_little_endian().await?;
      if meta_len > 4 * 1024 * 1024 {
        // 4MiB Limit for metadata
        return Err(AppError::InvalidRequest(String::from("metadata too large")).into());
      }
      if meta_len == 0 {
        break;
      }

      let mut meta_buffer = vec![0; meta_len as usize];
      payload_reader.read_exact(&mut meta_buffer).await?;
      serde_json::from_slice(&meta_buffer)?
    };

    let data = {
      let data_len = payload_reader.read_u32_little_endian().await?;
      if data_len > 32 * 1024 * 1024 {
        // 32MiB Limit for data
        return Err(AppError::InvalidRequest(String::from("data too large")).into());
      }
      let mut data_buffer = vec![0; data_len as usize];
      payload_reader.read_exact(&mut data_buffer).await?;
      data_buffer
    };

    accumulator.push(PublishCollabItem { meta, data });
  }

  if accumulator.is_empty() {
    return Err(
      AppError::InvalidRequest(String::from("did not receive any data to publish")).into(),
    );
  }
  state
    .published_collab_store
    .publish_collabs(accumulator, &workspace_id, &user_uuid)
    .await?;
  Ok(Json(AppResponse::Ok()))
}

async fn patch_published_collabs_handler(
  workspace_id: web::Path<Uuid>,
  user_uuid: UserUuid,
  state: Data<AppState>,
  patches: Json<Vec<PatchPublishedCollab>>,
) -> Result<Json<AppResponse<()>>> {
  let workspace_id = workspace_id.into_inner();
  if patches.is_empty() {
    return Err(AppError::InvalidRequest("No patches provided".to_string()).into());
  }
  state
    .published_collab_store
    .patch_collabs(&workspace_id, &user_uuid, &patches)
    .await?;
  Ok(Json(AppResponse::Ok()))
}

async fn delete_published_collabs_handler(
  workspace_id: web::Path<Uuid>,
  user_uuid: UserUuid,
  state: Data<AppState>,
  view_ids: Json<Vec<Uuid>>,
) -> Result<Json<AppResponse<()>>> {
  let workspace_id = workspace_id.into_inner();
  let view_ids = view_ids.into_inner();
  if view_ids.is_empty() {
    return Err(AppError::InvalidRequest("No view_ids provided".to_string()).into());
  }
  state
    .published_collab_store
    .delete_collabs(&workspace_id, &view_ids, &user_uuid)
    .await?;
  Ok(Json(AppResponse::Ok()))
}

#[instrument(level = "debug", skip(state, payload), err)]
async fn get_collab_member_list_handler(
  payload: Json<QueryCollabMembers>,
  state: Data<AppState>,
) -> Result<Json<AppResponse<AFCollabMembers>>> {
  let members =
    biz::collab::ops::get_collab_member_list(&state.pg_pool, &payload.into_inner()).await?;
  Ok(Json(AppResponse::Ok().with_data(AFCollabMembers(members))))
}

#[instrument(level = "info", skip_all, err)]
async fn post_realtime_message_stream_handler(
  user_uuid: UserUuid,
  mut payload: Payload,
  server: Data<RealtimeServerAddr>,
  state: Data<AppState>,
  req: HttpRequest,
) -> Result<Json<AppResponse<()>>> {
  // TODO(nathan): after upgrade the client application, then the device_id should not be empty
  let device_id = device_id_from_headers(req.headers()).unwrap_or_else(|_| "".to_string());
  let uid = state
    .user_cache
    .get_user_uid(&user_uuid)
    .await
    .map_err(AppResponseError::from)?;

  let mut bytes = BytesMut::new();
  while let Some(item) = payload.next().await {
    bytes.extend_from_slice(&item?);
  }

  event!(tracing::Level::INFO, "message len: {}", bytes.len());
  let device_id = device_id.to_string();

  let message = parser_realtime_msg(bytes.freeze(), req.clone()).await?;
  let stream_message = ClientStreamMessage {
    uid,
    device_id,
    message,
  };

  // When the server is under heavy load, try_send may fail. In client side, it will retry to send
  // the message later.
  match server.try_send(stream_message) {
    Ok(_) => return Ok(Json(AppResponse::Ok())),
    Err(err) => Err(
      AppError::Internal(anyhow!(
        "Failed to send message to websocket server, error:{}",
        err
      ))
      .into(),
    ),
  }
}

async fn get_workspace_usage_handler(
  user_uuid: UserUuid,
  workspace_id: web::Path<Uuid>,
  state: Data<AppState>,
) -> Result<Json<AppResponse<WorkspaceUsage>>> {
  let uid = state.user_cache.get_user_uid(&user_uuid).await?;
  state
    .workspace_access_control
    .enforce_role(&uid, &workspace_id.to_string(), AFRole::Owner)
    .await?;
  let res =
    biz::workspace::ops::get_workspace_document_total_bytes(&state.pg_pool, &workspace_id).await?;
  Ok(Json(AppResponse::Ok().with_data(res)))
}

async fn get_workspace_folder_handler(
  user_uuid: UserUuid,
  workspace_id: web::Path<Uuid>,
  state: Data<AppState>,
  query: web::Query<QueryWorkspaceFolder>,
) -> Result<Json<AppResponse<FolderView>>> {
  let depth = query.depth.unwrap_or(1);
  let uid = state.user_cache.get_user_uid(&user_uuid).await?;
  let workspace_id = workspace_id.into_inner();
  state
    .workspace_access_control
    .enforce_action(&uid, &workspace_id.to_string(), Action::Read)
    .await?;
  let root_view_id = if let Some(root_view_id) = query.root_view_id.as_ref() {
    root_view_id.to_string()
  } else {
    workspace_id.to_string()
  };
  let folder_view = biz::collab::ops::get_user_workspace_structure(
    &state.collab_access_control_storage,
    &state.pg_pool,
    uid,
    workspace_id,
    depth,
    &root_view_id,
  )
  .await?;
  Ok(Json(AppResponse::Ok().with_data(folder_view)))
}

async fn get_recent_views_handler(
  user_uuid: UserUuid,
  workspace_id: web::Path<Uuid>,
  state: Data<AppState>,
) -> Result<Json<AppResponse<RecentSectionItems>>> {
  let uid = state.user_cache.get_user_uid(&user_uuid).await?;
  let workspace_id = workspace_id.into_inner();
  state
    .workspace_access_control
    .enforce_action(&uid, &workspace_id.to_string(), Action::Read)
    .await?;
  let folder_views = get_user_recent_folder_views(
    &state.collab_access_control_storage,
    &state.pg_pool,
    uid,
    workspace_id,
  )
  .await?;
  let section_items = RecentSectionItems {
    views: folder_views,
  };
  Ok(Json(AppResponse::Ok().with_data(section_items)))
}

async fn get_favorite_views_handler(
  user_uuid: UserUuid,
  workspace_id: web::Path<Uuid>,
  state: Data<AppState>,
) -> Result<Json<AppResponse<FavoriteSectionItems>>> {
  let uid = state.user_cache.get_user_uid(&user_uuid).await?;
  let workspace_id = workspace_id.into_inner();
  state
    .workspace_access_control
    .enforce_action(&uid, &workspace_id.to_string(), Action::Read)
    .await?;
  let folder_views = get_user_favorite_folder_views(
    &state.collab_access_control_storage,
    &state.pg_pool,
    uid,
    workspace_id,
  )
  .await?;
  let section_items = FavoriteSectionItems {
    views: folder_views,
  };
  Ok(Json(AppResponse::Ok().with_data(section_items)))
}

async fn get_trash_views_handler(
  user_uuid: UserUuid,
  workspace_id: web::Path<Uuid>,
  state: Data<AppState>,
) -> Result<Json<AppResponse<TrashSectionItems>>> {
  let uid = state.user_cache.get_user_uid(&user_uuid).await?;
  let workspace_id = workspace_id.into_inner();
  state
    .workspace_access_control
    .enforce_action(&uid, &workspace_id.to_string(), Action::Read)
    .await?;
  let folder_views =
    get_user_trash_folder_views(&state.collab_access_control_storage, uid, workspace_id).await?;
  let section_items = TrashSectionItems {
    views: folder_views,
  };
  Ok(Json(AppResponse::Ok().with_data(section_items)))
}

async fn get_workspace_publish_outline_handler(
  publish_namespace: web::Path<String>,
  state: Data<AppState>,
) -> Result<Json<AppResponse<PublishedView>>> {
  let published_view = biz::collab::ops::get_published_view(
    &state.collab_access_control_storage,
    publish_namespace.into_inner(),
    &state.pg_pool,
  )
  .await?;
  Ok(Json(AppResponse::Ok().with_data(published_view)))
}

#[inline]
async fn parser_realtime_msg(
  payload: Bytes,
  req: HttpRequest,
) -> Result<RealtimeMessage, AppError> {
  let HttpRealtimeMessage {
    device_id: _,
    payload,
  } =
    HttpRealtimeMessage::decode(payload.as_ref()).map_err(|err| AppError::Internal(err.into()))?;
  let payload = match req.headers().get(X_COMPRESSION_TYPE) {
    None => payload,
    Some(_) => match compress_type_from_header_value(req.headers())? {
      CompressionType::Brotli { buffer_size } => {
        let decompressed_data = blocking_decompress(payload, buffer_size).await?;
        event!(
          tracing::Level::TRACE,
          "Decompress realtime http message with len: {}",
          decompressed_data.len()
        );
        decompressed_data
      },
    },
  };
  let message = Message::from(payload);
  match message {
    Message::Binary(bytes) => {
      let realtime_msg = tokio::task::spawn_blocking(move || {
        RealtimeMessage::decode(&bytes).map_err(|err| {
          AppError::InvalidRequest(format!("Failed to parse RealtimeMessage: {}", err))
        })
      })
      .await
      .map_err(AppError::from)??;
      Ok(realtime_msg)
    },
    _ => Err(AppError::InvalidRequest(format!(
      "Unsupported message type: {:?}",
      message
    ))),
  }
}

async fn fetch_embeddings(
  indexer_provider: &IndexerProvider,
  params: &mut [CollabParams],
) -> Result<(), AppError> {
  let mut futures = Vec::with_capacity(params.len());
  for param in params.iter() {
    let future = indexer_provider.create_collab_embeddings(param);
    futures.push(future);
  }

  let results = try_join_all(futures).await?;
  for (i, embeddings) in results.into_iter().enumerate() {
    params[i].embeddings = embeddings;
  }

  Ok(())
}
