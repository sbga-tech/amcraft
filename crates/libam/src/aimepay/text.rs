//! The texts of the Aime Pay busy page (`payment.md` PayApMgr 9): messages and the server error table.
//!
//! The Japanese strings are what the customer display shows (`payment.apmgr.disp.messages`); they are binary
//! data, not diagnostics, so they are reproduced verbatim.

/// Longest line the busy page keeps (`wchar[32]`).
const LINE_UNITS: usize = 31;

/// Idle run start and Activation start while the environment is not ready (`0x14023B110`, `0x140287270`).
pub const PLEASE_WAIT: &str = "しばらくお待ちください";
/// The environment is ready, or the store information is being fetched (`0x140286790`).
pub const FETCHING_STORE: &str = "店舗情報取得中です";
/// The Activation accept step (`0x1402868D0`).
pub const CONFIRM_STORE: &str = "店舗情報を確認してください";
/// Activation result, environment not ready.
pub const UNAVAILABLE: &str = "ただいまお取扱いできません";
/// Activation result, the record could not be saved.
pub const STORE_SAVE_FAILED: &str = "店舗情報の保存に失敗しました";
/// Activation result, saved.
pub const STORE_SAVED: &str = "店舗情報を保存しました";
/// Cancelled by the user (Activation and payment results).
pub const CANCELLED: &str = "取引が中止されました";
/// Deactivation result, success.
pub const STORE_DELETED: &str = "店舗情報を削除しました";
/// Deactivation result, failure.
pub const STORE_DELETE_FAILED: &str = "店舗情報の削除に失敗しました";
/// Payment start and PIN accepted.
pub const IN_PROGRESS: &str = "取引中です";
/// The PIN wait.
pub const CHECK_IN: &str = "チェックインしてください";
/// Payment result, success.
pub const SUCCEEDED: &str = "取引に成功しました";
/// Payment result, the history could not be saved.
pub const HISTORY_SAVE_FAILED: &str = "取引履歴の保存に失敗しました";
/// Payment result, the coins could not be added.
pub const COIN_ADD_FAILED: &str = "コインの追加に失敗しました";
/// The generic server failure title.
pub const FAILED: &str = "取引に失敗しました";

/// Result display waits in seconds: successes and cancellations, server errors.
pub const RESULT_WAIT: u64 = 3;
/// Server error display wait in seconds.
pub const ERROR_WAIT: u64 = 5;

/// A busy page line as the display stores it (at most 31 units).
fn line(text: &str) -> String {
    let mut units = 0;
    text.chars()
        .take_while(|character| {
            units += character.len_utf16();
            units <= LINE_UNITS
        })
        .collect()
}

/// The two lines for a failed server exchange (`0x140288520`). `kind` is the answer's `result_code` (0 after a
/// transport failure), `code` its `detail_code` (`None` while an answer is pending).
pub fn server_error(kind: u32, code: Option<&str>) -> (String, String) {
    if kind == 0 {
        return (line(FAILED), String::new());
    }
    let Some(code) = code else { return (line(FAILED), line(&kind.to_string())) };
    let title = match kind {
        10 if matches!(code, "AMPEWG1010" | "AMPEWG1011") => "メンテナンス情報を確認してください",
        20 => match code {
            "AMPEWL1017" => "このAimeはロックされています",
            "AMPEWL1018" => "クレジットカードが登録されていません",
            "AMPEWL1060" => "登録されたクレジットカードの有効期限が切れています",
            "AMPEWL1062" => "使用できないクレジットカードが登録されています",
            "AMPEWL1080" | "AMPEWL1081" | "AMPEWL2056" => "サービスが利用できません。サポートに問い合わせてください",
            "AMPEWL2031" => "上限金額設定を超えるため処理できません",
            "AMPEWL2050" | "AMPEWL2051" | "AMPEWL2052" | "AMPEWL2053" | "AMPEWL2054" => {
                "ユーザ情報が登録されていません"
            }
            "AMPEWL2060" => "チェックインコードが正しくありません",
            "AMPEWL2080" => "チェックインしなおしてください",
            "AMPEWL2082" => "チェックイン制限中です。詳細はサービスサイトを確認してください",
            "AMPEWL3042" => "購入数が超えたため購入できませんでした",
            _ => FAILED,
        },
        40 => match code.get(..3) {
            Some("E61" | "E90" | "E91" | "42C") => "取引に失敗しました。もう一度やり直してください",
            Some("42G") => "取引に失敗しました。カード会社に問い合わせてください",
            _ => FAILED,
        },
        _ => FAILED,
    };
    (line(title), line(&format!("{kind} - {code}")))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn server_error_titles_follow_the_result_code_class() {
        assert_eq!(server_error(0, Some("")), (FAILED.to_owned(), String::new()));
        assert_eq!(server_error(7, None), (FAILED.to_owned(), "7".to_owned()));
        assert_eq!(
            server_error(20, Some("AMPEWL2054")),
            ("ユーザ情報が登録されていません".to_owned(), "20 - AMPEWL2054".to_owned())
        );
        // the 40 class looks at the first three characters only
        assert_eq!(server_error(40, Some("42G123")).0, "取引に失敗しました。カード会社に問い合わせてください");
        // a maintenance code under another result code is the generic title
        assert_eq!(server_error(20, Some("AMPEWG1010")).0, FAILED);
    }

    #[test]
    fn busy_lines_keep_at_most_31_units() {
        assert_eq!(line(&"あ".repeat(40)), "あ".repeat(31));
        // the longest table title is exactly 31 characters and survives whole
        let (title, _) = server_error(20, Some("AMPEWL2082"));
        assert_eq!(title, "チェックイン制限中です。詳細はサービスサイトを確認してください");
    }
}
