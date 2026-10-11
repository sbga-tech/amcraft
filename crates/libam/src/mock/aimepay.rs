//! A scripted Aime Pay HTTP server for libam's payment flows.
//!
//! It answers the activation, item-list, check-in and payment routes and records each JSON request. Cloned
//! script handles let callers change check-in state and payment replies while libam owns the HTTP client.

use std::cell::{Cell, RefCell};
use std::rc::Rc;

use backend::{HttpClient, HttpError, HttpRequest, HttpResponse};

/// A local Aime Pay server with shared request and response script handles.
pub struct SimAimePayServer {
    /// Coin item published by the item-list route.
    pub item_id: &'static str,
    /// Requests in wire order, as endpoint paths and decoded JSON bodies.
    pub log: Rc<RefCell<Vec<(String, serde_json::Value)>>>,
    /// Whether the check-in route currently reports the user as checked in.
    pub checked_in: Rc<Cell<bool>>,
    /// JSON response returned by the play-fee payment route.
    pub pay_answer: Rc<RefCell<String>>,
}

impl HttpClient for SimAimePayServer {
    fn request(&mut self, request: &HttpRequest) -> Result<HttpResponse, HttpError> {
        let path = request.url.strip_prefix("http://127.0.0.1:1").expect("the root endpoint of env.json").to_owned();
        let body = serde_json::from_slice(&request.body).expect("a JSON request body");
        let answer = match path.as_str() {
            "/api/activation" => {
                r#"{"store_code":"1234","store_name":"Test Store","company_code":"99","company_name":"Test Corp"}"#
                    .to_owned()
            }
            "/api/cabitem/list" => format!(
                r#"{{"tax_per":10,"item_list":[{{"item_id":"{}","item_name":"coin","item_type":1,"price":100,
                "price_extax":91,"open_date":"2020-01-01 00:00:00","close_date":"2030-01-01 00:00:00"}}]}}"#,
                self.item_id
            ),
            "/api/amountprice" => r#"{"history":[]}"#.to_owned(),
            "/api/chkamp" => format!(r#"{{"credit_flg":true,"checkin_flg":{}}}"#, self.checked_in.get()),
            "/api/chkpin" => {
                self.checked_in.set(true);
                "{}".to_owned()
            }
            "/api/payment/playfee" => self.pay_answer.borrow().clone(),
            _ => "{}".to_owned(),
        };
        self.log.borrow_mut().push((path, body));
        Ok(HttpResponse { status: 200, headers: Vec::new(), body: answer.into_bytes() })
    }
}
