use axum::{extract::Request, http::StatusCode, middleware::Next, response::Response};

#[derive(Debug, Clone)]
pub struct EmailCheck {
    env: String,
    admin_email: String,
    whitelist: Vec<String>,
}

// TODO(caio): I think tailscale-user is the most stable way to authorize
// to do that we must find for each guest their tailscale user ID, which I haven't figured out
// how to find yet
impl EmailCheck {
    pub fn new(admin_email: String, whitelist: Vec<String>, env: String) -> Self {
        Self {
            admin_email,
            whitelist,
            env,
        }
    }

    pub async fn admin_middleware(
        self,
        mut req: Request,
        next: Next,
    ) -> Result<Response, StatusCode> {
        if self.env == "local" {
            return Ok(next.run(req).await);
        }

        let user_email = req
            .headers()
            .get("tailscale-user-login")
            .and_then(|h| h.to_str().ok())
            .map(|s| s.to_string())
            .ok_or(StatusCode::UNAUTHORIZED)?;

        if user_email != self.admin_email {
            return Err(StatusCode::FORBIDDEN);
        }

        req.extensions_mut().insert(user_email);
        Ok(next.run(req).await)
    }

    pub async fn guest_middleware(
        self,
        mut req: Request,
        next: Next,
    ) -> Result<Response, StatusCode> {
        if self.env == "local" {
            return Ok(next.run(req).await);
        }

        let user_email = req
            .headers()
            .get("tailscale-user-login")
            .and_then(|h| h.to_str().ok())
            .map(|s| s.to_string())
            .ok_or(StatusCode::UNAUTHORIZED)?;

        if user_email != self.admin_email && !self.whitelist.contains(&user_email) {
            return Err(StatusCode::FORBIDDEN);
        }

        req.extensions_mut().insert(user_email);
        Ok(next.run(req).await)
    }
}
