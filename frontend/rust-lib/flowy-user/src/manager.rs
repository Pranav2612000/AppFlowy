use std::path::PathBuf;
use std::string::ToString;
use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::{Arc, Weak};

use base64::alphabet::URL_SAFE;
use base64::engine::general_purpose::PAD;
use base64::engine::GeneralPurpose;
use collab_user::core::MutexUserAwareness;
use serde_json::Value;
use tokio::sync::{Mutex, RwLock};
use tokio_stream::StreamExt;
use tracing::{debug, error, event, info, instrument};

use collab_integrate::collab_builder::AppFlowyCollabBuilder;
use collab_integrate::RocksCollabDB;
use flowy_error::{internal_error, ErrorCode, FlowyResult};
use flowy_sqlite::kv::StorePreferences;
use flowy_sqlite::schema::user_table;
use flowy_sqlite::ConnectionPool;
use flowy_sqlite::{query_dsl::*, DBConnection, ExpressionMethods};
use flowy_user_deps::cloud::UserUpdate;
use flowy_user_deps::entities::*;
use lib_dispatch::prelude::af_spawn;
use lib_infra::box_any::BoxAny;

use crate::anon_user_upgrade::{migration_anon_user_on_sign_up, sync_user_data_to_cloud};
use crate::entities::{AuthStateChangedPB, AuthStatePB, UserProfilePB, UserSettingPB};
use crate::event_map::{DefaultUserStatusCallback, UserCloudServiceProvider, UserStatusCallback};
use crate::migrations::document_empty_content::HistoricalEmptyDocumentMigration;
use crate::migrations::migration::{UserDataMigration, UserLocalDataMigration};
use crate::migrations::workspace_and_favorite_v1::FavoriteV1AndWorkspaceArrayMigration;
use crate::migrations::MigrationUser;
use crate::services::cloud_config::get_cloud_config;
use crate::services::collab_interact::{CollabInteract, DefaultCollabInteract};
use crate::services::database::{UserDB, UserDBPath};
use crate::services::entities::{ResumableSignUp, Session};
use crate::services::user_awareness::UserAwarenessDataSource;
use crate::services::user_sql::{UserTable, UserTableChangeset};
use crate::services::user_workspace::save_user_workspaces;
use crate::{errors::FlowyError, notification::*};

pub const URL_SAFE_ENGINE: GeneralPurpose = GeneralPurpose::new(&URL_SAFE, PAD);
pub struct UserSessionConfig {
  root_dir: String,
  /// Used as the key of `Session` when saving session information to KV.
  session_cache_key: String,
}

impl UserSessionConfig {
  /// The `root_dir` represents as the root of the user folders. It must be unique for each
  /// users.
  pub fn new(name: &str, root_dir: &str) -> Self {
    let session_cache_key = format!("{}_session_cache", name);
    Self {
      root_dir: root_dir.to_owned(),
      session_cache_key,
    }
  }
}

pub struct UserManager {
  database: Arc<UserDB>,
  user_paths: UserPaths,
  session_config: UserSessionConfig,
  pub(crate) cloud_services: Arc<dyn UserCloudServiceProvider>,
  pub(crate) store_preferences: Arc<StorePreferences>,
  pub(crate) user_awareness: Arc<Mutex<Option<MutexUserAwareness>>>,
  pub(crate) user_status_callback: RwLock<Arc<dyn UserStatusCallback>>,
  pub(crate) collab_builder: Weak<AppFlowyCollabBuilder>,
  pub(crate) collab_interact: RwLock<Arc<dyn CollabInteract>>,
  resumable_sign_up: Mutex<Option<ResumableSignUp>>,
  current_session: Arc<parking_lot::RwLock<Option<Session>>>,
  refresh_user_profile_since: AtomicI64,
}

impl UserManager {
  pub fn new(
    session_config: UserSessionConfig,
    cloud_services: Arc<dyn UserCloudServiceProvider>,
    store_preferences: Arc<StorePreferences>,
    collab_builder: Weak<AppFlowyCollabBuilder>,
  ) -> Arc<Self> {
    let user_paths = UserPaths {
      root: session_config.root_dir.clone(),
    };
    let database = Arc::new(UserDB::new(user_paths.clone()));
    let user_status_callback: RwLock<Arc<dyn UserStatusCallback>> =
      RwLock::new(Arc::new(DefaultUserStatusCallback));

    let refresh_user_profile_since = AtomicI64::new(0);
    let user_manager = Arc::new(Self {
      database,
      user_paths,
      session_config,
      cloud_services,
      store_preferences,
      user_awareness: Arc::new(Default::default()),
      user_status_callback,
      collab_builder,
      collab_interact: RwLock::new(Arc::new(DefaultCollabInteract)),
      resumable_sign_up: Default::default(),
      current_session: Default::default(),
      refresh_user_profile_since,
    });

    let weak_user_manager = Arc::downgrade(&user_manager);
    if let Ok(user_service) = user_manager.cloud_services.get_user_service() {
      if let Some(mut rx) = user_service.subscribe_user_update() {
        af_spawn(async move {
          while let Some(update) = rx.recv().await {
            if let Some(user_manager) = weak_user_manager.upgrade() {
              if let Err(err) = user_manager.handler_user_update(update).await {
                error!("handler_user_update failed: {:?}", err);
              }
            }
          }
        });
      }
    }

    user_manager
  }

  pub fn get_store_preferences(&self) -> Weak<StorePreferences> {
    Arc::downgrade(&self.store_preferences)
  }

  /// Initializes the user session, including data migrations and user awareness configuration. This function
  /// will be invoked each time the user opens the application.
  ///
  /// Starts by retrieving the current session. If the session is successfully obtained, it will attempt
  /// a local data migration for the user. After ensuring the user's data is migrated and up-to-date,
  /// the function will set up the collaboration configuration and initialize the user's awareness. Upon successful
  /// completion, a user status callback is invoked to signify that the initialization process is complete.
  #[instrument(level = "debug", skip_all, err)]
  pub async fn init<C: UserStatusCallback + 'static, I: CollabInteract>(
    &self,
    user_status_callback: C,
    collab_interact: I,
  ) -> Result<(), FlowyError> {
    let user_status_callback = Arc::new(user_status_callback);
    *self.user_status_callback.write().await = user_status_callback.clone();
    *self.collab_interact.write().await = Arc::new(collab_interact);

    if let Ok(session) = self.get_session() {
      let user = self.get_user_profile(session.user_id).await?;

      event!(
        tracing::Level::INFO,
        "init user session: {}:{}",
        user.uid,
        user.email
      );

      // Set the token if the current cloud service using token to authenticate
      // Currently, only the AppFlowy cloud using token to init the client api.
      if let Err(err) = self.cloud_services.set_token(&user.token) {
        error!("Set token failed: {}", err);
      }

      // Subscribe the token state
      let weak_pool = Arc::downgrade(&self.db_pool(user.uid)?);
      if let Some(mut token_state_rx) = self.cloud_services.subscribe_token_state() {
        event!(tracing::Level::DEBUG, "Listen token state change");
        af_spawn(async move {
          while let Some(token_state) = token_state_rx.next().await {
            debug!("Token state changed: {:?}", token_state);
            match token_state {
              UserTokenState::Refresh { token } => {
                // Only save the token if the token is different from the current token
                if token != user.token {
                  if let Some(pool) = weak_pool.upgrade() {
                    // Save the new token
                    if let Err(err) = save_user_token(user.uid, pool, token) {
                      error!("Save user token failed: {}", err);
                    }
                  }
                }
              },
              UserTokenState::Invalid => {},
            }
          }
        });
      }

      // Do the user data migration if needed
      event!(tracing::Level::INFO, "Prepare user data migration");
      match (
        self.database.get_collab_db(session.user_id),
        self.database.get_pool(session.user_id),
      ) {
        (Ok(collab_db), Ok(sqlite_pool)) => {
          // ⚠️The order of migrations is crucial. If you're adding a new migration, please ensure
          // it's appended to the end of the list.
          let migrations: Vec<Box<dyn UserDataMigration>> = vec![
            Box::new(HistoricalEmptyDocumentMigration),
            Box::new(FavoriteV1AndWorkspaceArrayMigration),
          ];
          match UserLocalDataMigration::new(session.clone(), collab_db, sqlite_pool).run(migrations)
          {
            Ok(applied_migrations) => {
              if !applied_migrations.is_empty() {
                info!("Did apply migrations: {:?}", applied_migrations);
              }
            },
            Err(e) => error!("User data migration failed: {:?}", e),
          }
        },
        _ => error!("Failed to get collab db or sqlite pool"),
      }
      self.set_collab_config(&session);
      // Init the user awareness
      self
        .initialize_user_awareness(&session, UserAwarenessDataSource::Local)
        .await;

      let cloud_config = get_cloud_config(session.user_id, &self.store_preferences);
      if let Err(e) = user_status_callback
        .did_init(
          session.user_id,
          &cloud_config,
          &session.user_workspace,
          &session.device_id,
        )
        .await
      {
        error!("Failed to call did_init callback: {:?}", e);
      }
    }
    Ok(())
  }

  pub fn db_connection(&self, uid: i64) -> Result<DBConnection, FlowyError> {
    self.database.get_connection(uid)
  }

  pub fn db_pool(&self, uid: i64) -> Result<Arc<ConnectionPool>, FlowyError> {
    self.database.get_pool(uid)
  }

  pub fn get_collab_db(&self, uid: i64) -> Result<Weak<RocksCollabDB>, FlowyError> {
    self
      .database
      .get_collab_db(uid)
      .map(|collab_db| Arc::downgrade(&collab_db))
  }

  /// Performs a user sign-in, initializing user awareness and sending relevant notifications.
  ///
  /// This asynchronous function interacts with an external user service to authenticate and sign in a user
  /// based on provided parameters. Once signed in, it updates the collaboration configuration, logs the user,
  /// saves their workspaces, and initializes their user awareness.
  ///
  /// A sign-in notification is also sent after a successful sign-in.
  ///
  #[tracing::instrument(level = "debug", skip(self, params))]
  pub async fn sign_in(
    &self,
    params: BoxAny,
    authenticator: Authenticator,
  ) -> Result<UserProfile, FlowyError> {
    self.update_authenticator(&authenticator).await;
    let response: AuthResponse = self
      .cloud_services
      .get_user_service()?
      .sign_in(params)
      .await?;
    let session = Session::from(&response);
    self.set_collab_config(&session);

    let latest_workspace = response.latest_workspace.clone();
    let user_profile = UserProfile::from((&response, &authenticator));
    self
      .save_auth_data(&response, &authenticator, &session)
      .await?;
    let _ = self
      .initialize_user_awareness(&session, UserAwarenessDataSource::Remote)
      .await;

    if let Err(e) = self
      .user_status_callback
      .read()
      .await
      .did_sign_in(user_profile.uid, &latest_workspace, &session.device_id)
      .await
    {
      error!("Failed to call did_sign_in callback: {:?}", e);
    }
    send_auth_state_notification(AuthStateChangedPB {
      state: AuthStatePB::AuthStateSignIn,
      message: "Sign in success".to_string(),
    })
    .send();
    Ok(user_profile)
  }

  pub(crate) async fn update_authenticator(&self, authenticator: &Authenticator) {
    self
      .user_status_callback
      .read()
      .await
      .authenticator_did_changed(authenticator.clone());
    self.cloud_services.set_authenticator(authenticator.clone());
  }

  /// Manages the user sign-up process, potentially migrating data if necessary.
  ///
  /// This asynchronous function interacts with an external authentication service to register and sign up a user
  /// based on the provided parameters. Following a successful sign-up, it handles configuration updates, logging,
  /// and saving workspace information. If a user is signing up with a new profile and previously had guest data,
  /// this function may migrate that data over to the new account.
  ///
  #[tracing::instrument(level = "info", skip(self, params))]
  pub async fn sign_up(
    &self,
    authenticator: Authenticator,
    params: BoxAny,
  ) -> Result<UserProfile, FlowyError> {
    self.update_authenticator(&authenticator).await;

    let migration_user = self.get_migration_user(&authenticator).await;
    let auth_service = self.cloud_services.get_user_service()?;
    let response: AuthResponse = auth_service.sign_up(params).await?;
    let user_profile = UserProfile::from((&response, &authenticator));
    if user_profile.encryption_type.is_need_encrypt_secret() {
      self
        .resumable_sign_up
        .lock()
        .await
        .replace(ResumableSignUp {
          user_profile: user_profile.clone(),
          migration_user,
          response,
          authenticator,
        });
    } else {
      self
        .continue_sign_up(&user_profile, migration_user, response, &authenticator)
        .await?;
    }
    Ok(user_profile)
  }

  #[tracing::instrument(level = "info", skip(self))]
  pub async fn resume_sign_up(&self) -> Result<(), FlowyError> {
    let ResumableSignUp {
      user_profile,
      migration_user,
      response,
      authenticator,
    } = self
      .resumable_sign_up
      .lock()
      .await
      .clone()
      .ok_or(FlowyError::new(
        ErrorCode::Internal,
        "No resumable sign up data",
      ))?;
    self
      .continue_sign_up(&user_profile, migration_user, response, &authenticator)
      .await?;
    Ok(())
  }

  #[tracing::instrument(level = "info", skip_all, err)]
  async fn continue_sign_up(
    &self,
    user_profile: &UserProfile,
    migration_user: Option<MigrationUser>,
    response: AuthResponse,
    authenticator: &Authenticator,
  ) -> FlowyResult<()> {
    let new_session = Session::from(&response);
    self.set_collab_config(&new_session);

    let user_awareness_source = if response.is_new_user {
      UserAwarenessDataSource::Local
    } else {
      UserAwarenessDataSource::Remote
    };

    if response.is_new_user {
      if let Some(old_user) = migration_user {
        let new_user = MigrationUser {
          user_profile: user_profile.clone(),
          session: new_session.clone(),
        };
        event!(
          tracing::Level::INFO,
          "Migrate anon user data from {:?} to {:?}",
          old_user.user_profile.uid,
          new_user.user_profile.uid
        );
        self
          .migrate_anon_user_data_to_cloud(&old_user, &new_user)
          .await?;
        let _ = self.database.close(old_user.session.user_id);
      }
    }
    self
      .initialize_user_awareness(&new_session, user_awareness_source)
      .await;

    self
      .save_auth_data(&response, authenticator, &new_session)
      .await?;

    self
      .user_status_callback
      .read()
      .await
      .did_sign_up(
        response.is_new_user,
        user_profile,
        &new_session.user_workspace,
        &new_session.device_id,
      )
      .await?;

    send_auth_state_notification(AuthStateChangedPB {
      state: AuthStatePB::AuthStateSignIn,
      message: "Sign up success".to_string(),
    })
    .send();
    Ok(())
  }

  #[tracing::instrument(level = "info", skip(self))]
  pub async fn sign_out(&self) -> Result<(), FlowyError> {
    let session = self.get_session()?;
    self.database.close(session.user_id)?;
    self.set_session(None)?;

    let server = self.cloud_services.get_user_service()?;
    af_spawn(async move {
      if let Err(err) = server.sign_out(None).await {
        event!(tracing::Level::ERROR, "{:?}", err);
      }
    });
    Ok(())
  }

  /// Updates the user's profile with the given parameters.
  ///
  /// This function modifies the user's profile based on the provided update parameters. After updating, it
  /// sends a notification about the change. It's also responsible for handling interactions with the underlying
  /// database and updates user profile.
  ///
  #[tracing::instrument(level = "debug", skip(self))]
  pub async fn update_user_profile(
    &self,
    params: UpdateUserProfileParams,
  ) -> Result<(), FlowyError> {
    let changeset = UserTableChangeset::new(params.clone());
    let session = self.get_session()?;
    upsert_user_profile_change(session.user_id, self.db_pool(session.user_id)?, changeset)?;

    let profile = self.get_user_profile(session.user_id).await?;
    self
      .update_user(session.user_id, profile.token, params)
      .await?;
    Ok(())
  }

  pub async fn init_user(&self) -> Result<(), FlowyError> {
    Ok(())
  }

  /// Fetches the user profile for the given user ID.
  pub async fn get_user_profile(&self, uid: i64) -> Result<UserProfile, FlowyError> {
    let user: UserProfile = user_table::dsl::user_table
      .filter(user_table::id.eq(&uid.to_string()))
      .first::<UserTable>(&*(self.db_connection(uid)?))
      .map_err(|err| {
        FlowyError::record_not_found().with_context(format!(
          "Can't find the user profile for user id: {}, error: {:?}",
          uid, err
        ))
      })?
      .into();

    Ok(user)
  }

  #[tracing::instrument(level = "info", skip_all, err)]
  pub async fn refresh_user_profile(&self, old_user_profile: &UserProfile) -> FlowyResult<()> {
    let now = chrono::Utc::now().timestamp();

    // Add debounce to avoid too many requests
    if now - self.refresh_user_profile_since.load(Ordering::SeqCst) < 5 {
      return Ok(());
    }

    self.refresh_user_profile_since.store(now, Ordering::SeqCst);
    let uid = old_user_profile.uid;
    let result: Result<UserProfile, FlowyError> = self
      .cloud_services
      .get_user_service()?
      .get_user_profile(UserCredentials::from_uid(uid))
      .await;

    match result {
      Ok(new_user_profile) => {
        // If the authentication type has changed, it indicates that the user has signed in
        // using a different release package but is sharing the same data folder.
        // In such cases, notify the frontend to log out.
        if old_user_profile.authenticator != Authenticator::Local
          && new_user_profile.authenticator != old_user_profile.authenticator
        {
          event!(
            tracing::Level::INFO,
            "User login with different authenticator: {:?} -> {:?}",
            old_user_profile.authenticator,
            new_user_profile.authenticator
          );

          send_auth_state_notification(AuthStateChangedPB {
            state: AuthStatePB::InvalidAuth,
            message: "User login with different cloud".to_string(),
          })
          .send();
          return Ok(());
        }

        // If the user profile is updated, save the new user profile
        if new_user_profile.updated_at > old_user_profile.updated_at {
          validate_encryption_sign(old_user_profile, &new_user_profile.encryption_type.sign());
          // Save the new user profile
          let changeset = UserTableChangeset::from_user_profile(new_user_profile);
          let _ = upsert_user_profile_change(uid, self.database.get_pool(uid)?, changeset);
        }
        Ok(())
      },
      Err(err) => {
        // If the user is not found, notify the frontend to logout
        if err.is_unauthorized() {
          event!(
            tracing::Level::ERROR,
            "User is unauthorized, sign out the user"
          );
          self.sign_out().await?;
          send_auth_state_notification(AuthStateChangedPB {
            state: AuthStatePB::InvalidAuth,
            message: "User is not found on the server".to_string(),
          })
          .send();
        }
        Err(err)
      },
    }
  }

  #[instrument(level = "info", skip_all)]
  pub fn user_dir(&self, uid: i64) -> String {
    self.user_paths.user_dir(uid)
  }

  pub fn user_setting(&self) -> Result<UserSettingPB, FlowyError> {
    let session = self.get_session()?;
    let user_setting = UserSettingPB {
      user_folder: self.user_dir(session.user_id),
    };
    Ok(user_setting)
  }

  pub fn user_id(&self) -> Result<i64, FlowyError> {
    Ok(self.get_session()?.user_id)
  }

  pub fn workspace_id(&self) -> Result<String, FlowyError> {
    Ok(self.get_session()?.user_workspace.id)
  }

  pub fn token(&self) -> Result<Option<String>, FlowyError> {
    Ok(None)
  }

  async fn update_user(
    &self,
    uid: i64,
    token: String,
    params: UpdateUserProfileParams,
  ) -> Result<(), FlowyError> {
    let server = self.cloud_services.get_user_service()?;
    af_spawn(async move {
      let credentials = UserCredentials::new(Some(token), Some(uid), None);
      server.update_user(credentials, params).await
    })
    .await
    .map_err(internal_error)??;
    Ok(())
  }

  async fn save_user(&self, uid: i64, user: UserTable) -> Result<(), FlowyError> {
    let conn = self.db_connection(uid)?;
    conn.immediate_transaction(|| {
      // delete old user if exists
      diesel::delete(user_table::dsl::user_table.filter(user_table::dsl::id.eq(&user.id)))
        .execute(&*conn)?;

      let _ = diesel::insert_into(user_table::table)
        .values(user)
        .execute(&*conn)?;
      Ok::<(), FlowyError>(())
    })?;

    Ok(())
  }

  pub async fn receive_realtime_event(&self, json: Value) {
    if let Ok(user_service) = self.cloud_services.get_user_service() {
      user_service.receive_realtime_event(json)
    }
  }

  /// Returns the current user session.
  pub fn get_session(&self) -> Result<Session, FlowyError> {
    if let Some(session) = (self.current_session.read()).clone() {
      return Ok(session);
    }

    match self
      .store_preferences
      .get_object::<Session>(&self.session_config.session_cache_key)
    {
      None => Err(FlowyError::new(
        ErrorCode::RecordNotFound,
        "User is not logged in",
      )),
      Some(session) => {
        self.current_session.write().replace(session.clone());
        Ok(session)
      },
    }
  }

  pub(crate) fn set_session(&self, session: Option<Session>) -> Result<(), FlowyError> {
    debug!("Set current user: {:?}", session);
    match &session {
      None => {
        self.current_session.write().take();
        self
          .store_preferences
          .remove(&self.session_config.session_cache_key)
      },
      Some(session) => {
        self.current_session.write().replace(session.clone());
        self
          .store_preferences
          .set_object(&self.session_config.session_cache_key, session.clone())
          .map_err(internal_error)?;
      },
    }
    Ok(())
  }

  pub(crate) async fn generate_sign_in_url_with_email(
    &self,
    authenticator: &Authenticator,
    email: &str,
  ) -> Result<String, FlowyError> {
    self.update_authenticator(authenticator).await;

    let auth_service = self.cloud_services.get_user_service()?;
    let url = auth_service
      .generate_sign_in_url_with_email(email)
      .await
      .map_err(|err| FlowyError::server_error().with_context(err))?;
    Ok(url)
  }

  pub(crate) async fn generate_oauth_url(
    &self,
    oauth_provider: &str,
  ) -> Result<String, FlowyError> {
    self.update_authenticator(&Authenticator::AFCloud).await;
    let auth_service = self.cloud_services.get_user_service()?;
    let url = auth_service
      .generate_oauth_url_with_provider(oauth_provider)
      .await?;
    Ok(url)
  }

  #[instrument(level = "info", skip_all, err)]
  async fn save_auth_data(
    &self,
    response: &impl UserAuthResponse,
    authenticator: &Authenticator,
    session: &Session,
  ) -> Result<(), FlowyError> {
    let user_profile = UserProfile::from((response, authenticator));
    let uid = user_profile.uid;
    event!(tracing::Level::DEBUG, "Save new history user: {:?}", uid);
    self.add_historical_user(
      uid,
      response.device_id(),
      response.user_name().to_string(),
      authenticator,
      self.user_dir(uid),
    );
    event!(tracing::Level::DEBUG, "Save new history user workspace");
    save_user_workspaces(uid, self.db_pool(uid)?, response.user_workspaces())?;
    event!(tracing::Level::INFO, "Save new user profile to disk");
    self
      .save_user(uid, (user_profile, authenticator.clone()).into())
      .await?;
    self.set_session(Some(session.clone()))?;
    Ok(())
  }

  fn set_collab_config(&self, session: &Session) {
    let collab_builder = self.collab_builder.upgrade().unwrap();
    collab_builder.set_sync_device(session.device_id.clone());
    collab_builder.initialize(session.user_workspace.id.clone());
    self.cloud_services.set_device_id(&session.device_id);
  }

  async fn handler_user_update(&self, user_update: UserUpdate) -> FlowyResult<()> {
    let session = self.get_session()?;
    if session.user_id == user_update.uid {
      debug!("Receive user update: {:?}", user_update);
      let user_profile = self.get_user_profile(user_update.uid).await?;
      if !validate_encryption_sign(&user_profile, &user_update.encryption_sign) {
        return Ok(());
      }

      // Save the user profile change
      upsert_user_profile_change(
        user_update.uid,
        self.db_pool(user_update.uid)?,
        UserTableChangeset::from(user_update),
      )?;
    }

    Ok(())
  }

  async fn migrate_anon_user_data_to_cloud(
    &self,
    old_user: &MigrationUser,
    new_user: &MigrationUser,
  ) -> Result<(), FlowyError> {
    let old_collab_db = self.database.get_collab_db(old_user.session.user_id)?;
    let new_collab_db = self.database.get_collab_db(new_user.session.user_id)?;
    migration_anon_user_on_sign_up(old_user, &old_collab_db, new_user, &new_collab_db)?;

    if let Err(err) = sync_user_data_to_cloud(
      self.cloud_services.get_user_service()?,
      "",
      new_user,
      &new_collab_db,
    )
    .await
    {
      error!("Sync user data to cloud failed: {:?}", err);
    }

    // Save the old user workspace setting.
    save_user_workspaces(
      old_user.session.user_id,
      self.database.get_pool(old_user.session.user_id)?,
      &[old_user.session.user_workspace.clone()],
    )?;
    Ok(())
  }
}

fn validate_encryption_sign(user_profile: &UserProfile, encryption_sign: &str) -> bool {
  // If the local user profile's encryption sign is not equal to the user update's encryption sign,
  // which means the user enable encryption in another device, we should logout the current user.
  let is_valid = user_profile.encryption_type.sign() == encryption_sign;
  if !is_valid {
    send_auth_state_notification(AuthStateChangedPB {
      state: AuthStatePB::InvalidAuth,
      message: "Encryption configuration was changed".to_string(),
    })
    .send();
  }
  is_valid
}

fn upsert_user_profile_change(
  uid: i64,
  pool: Arc<ConnectionPool>,
  changeset: UserTableChangeset,
) -> FlowyResult<()> {
  event!(
    tracing::Level::DEBUG,
    "Update user profile with changeset: {:?}",
    changeset
  );
  let conn = pool.get()?;
  diesel_update_table!(user_table, changeset, &*conn);
  let user: UserProfile = user_table::dsl::user_table
    .filter(user_table::id.eq(&uid.to_string()))
    .first::<UserTable>(&*conn)?
    .into();
  send_notification(&uid.to_string(), UserNotification::DidUpdateUserProfile)
    .payload(UserProfilePB::from(user))
    .send();
  Ok(())
}

#[instrument(level = "info", skip_all, err)]
fn save_user_token(uid: i64, pool: Arc<ConnectionPool>, token: String) -> FlowyResult<()> {
  let params = UpdateUserProfileParams::new(uid).with_token(token);
  let changeset = UserTableChangeset::new(params);
  upsert_user_profile_change(uid, pool, changeset)
}

#[derive(Clone)]
struct UserPaths {
  root: String,
}

impl UserPaths {
  fn user_dir(&self, uid: i64) -> String {
    format!("{}/{}", self.root, uid)
  }
}

impl UserDBPath for UserPaths {
  fn user_db_path(&self, uid: i64) -> PathBuf {
    PathBuf::from(self.user_dir(uid))
  }

  fn collab_db_path(&self, uid: i64) -> PathBuf {
    let mut path = PathBuf::from(self.user_dir(uid));
    path.push("collab_db");
    path
  }
}
