use crate::db;
use crate::i18n::Lang;
use crate::state::AppState;

pub async fn resolve_lang(state: &AppState, user_id: i64, client_lang_code: Option<&str>) -> Lang {
    if let Ok(Some(v)) = db::get_setting(&state.db, &Lang::setting_key(user_id)).await {
        return Lang::from_db_value(&v);
    }
    Lang::from_language_code(client_lang_code)
}
