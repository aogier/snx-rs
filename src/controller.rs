use std::{collections::VecDeque, str::FromStr, sync::Arc, time::Duration};

use anyhow::anyhow;
use directories_next::ProjectDirs;

use crate::ccc::CccHttpClient;
use crate::model::{MfaChallenge, MfaType};
use crate::prompt::{run_otp_listener, OTP_TIMEOUT};
use crate::{
    model::{params::TunnelParams, ConnectionStatus, TunnelServiceRequest, TunnelServiceResponse},
    platform::{self, UdpSocketExt},
    prompt::SecurePrompt,
    server_info,
};

const RECV_TIMEOUT: Duration = Duration::from_secs(2);
const CONNECT_TIMEOUT: Duration = Duration::from_secs(120);

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum ServiceCommand {
    Status,
    Connect,
    Disconnect,
    Reconnect,
    Info,
}

impl FromStr for ServiceCommand {
    type Err = anyhow::Error;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.to_lowercase().as_str() {
            "status" => Ok(Self::Status),
            "connect" => Ok(Self::Connect),
            "disconnect" => Ok(Self::Disconnect),
            "reconnect" => Ok(Self::Reconnect),
            "info" => Ok(Self::Info),
            other => Err(anyhow!("Invalid command: {}", other)),
        }
    }
}

pub struct ServiceController {
    pub params: TunnelParams,
    prompt: SecurePrompt,
    pwd_prompts: Option<VecDeque<String>>,
}

impl ServiceController {
    pub fn with_params(params: TunnelParams) -> Self {
        Self {
            params,
            prompt: SecurePrompt::tty(),
            pwd_prompts: None,
        }
    }

    pub fn new(prompt: SecurePrompt) -> anyhow::Result<Self> {
        let dir = ProjectDirs::from("", "", "snx-rs").ok_or(anyhow!("No project directory!"))?;
        let config_file = dir.config_dir().join("snx-rs.conf");

        if !config_file.exists() {
            return Err(anyhow!("No config file: {}", config_file.display()));
        }
        let mut params = TunnelParams::load(config_file)?;

        params.decode_password()?;

        Ok(Self {
            params,
            prompt,
            pwd_prompts: None,
        })
    }

    pub async fn command(&mut self, command: ServiceCommand) -> anyhow::Result<ConnectionStatus> {
        match command {
            ServiceCommand::Status => self.do_status().await,
            ServiceCommand::Connect => {
                self.fill_pwd_prompts().await.unwrap_or_default();
                self.do_status().await?;
                self.do_connect().await
            }
            ServiceCommand::Disconnect => {
                self.do_status().await?;
                self.do_disconnect().await
            }
            ServiceCommand::Reconnect => {
                let _ = self.do_disconnect().await;
                self.fill_pwd_prompts().await.unwrap_or_default();
                self.do_connect().await
            }
            ServiceCommand::Info => self.do_info().await,
        }
    }

    #[async_recursion::async_recursion]
    pub async fn do_status(&mut self) -> anyhow::Result<ConnectionStatus> {
        let response = self.send_receive(TunnelServiceRequest::GetStatus, RECV_TIMEOUT).await;
        match response {
            Ok(TunnelServiceResponse::ConnectionStatus(status)) => {
                if let (None, Some(mfa)) = (status.connected_since, &status.mfa) {
                    let input = self.get_mfa_input(mfa).await?;
                    self.do_challenge_code(input).await
                } else {
                    if status.connected_since.is_some() && !self.params.password.is_empty() && !self.params.no_keychain
                    {
                        let _ = platform::store_password(&self.params.user_name, &self.params.password).await;
                    }
                    Ok(status)
                }
            }
            Ok(_) => Err(anyhow!("Invalid response!")),
            Err(e) => Err(e),
        }
    }

    async fn get_mfa_input(&self, mfa: &MfaChallenge) -> anyhow::Result<String> {
        match mfa.mfa_type {
            MfaType::UserInput => self.prompt.get_secure_input(mfa.prompt.as_str()),
            MfaType::SamlSso => {
                opener::open(&mfa.prompt)?;
                Ok(tokio::time::timeout(OTP_TIMEOUT, run_otp_listener()).await??)
            }
        }
    }

    async fn do_connect(&mut self) -> anyhow::Result<ConnectionStatus> {
        let mut params = self.params.clone();

        if params.server_name.is_empty() || params.login_type.is_empty() {
            return Err(anyhow!(
                "Missing required parameters in the config file: server name and/or login type"
            ));
        }

        if params.password.is_empty() && params.client_cert.is_none() {
            if !params.no_keychain {
                if let Ok(password) = platform::acquire_password(&params.user_name).await {
                    params.password = password;
                }
            } else {
                let prompt = self
                    .pwd_prompts
                    .as_mut()
                    .and_then(|deque| deque.pop_front())
                    .unwrap_or(format!("Enter password for {}: ", params.user_name));
                params.password = self.prompt.get_secure_input(&prompt)?.trim().to_owned();
            }
            self.params = params;
        }

        let response = self
            .send_receive(TunnelServiceRequest::Connect(self.params.clone()), CONNECT_TIMEOUT)
            .await;
        match response {
            Ok(TunnelServiceResponse::Ok) => self.do_status().await,
            Ok(TunnelServiceResponse::Error(error)) => Err(anyhow!(error)),
            Ok(_) => Err(anyhow!("Invalid response!")),
            Err(e) => Err(e),
        }
    }

    async fn do_challenge_code(&mut self, code: String) -> anyhow::Result<ConnectionStatus> {
        let response = self
            .send_receive(
                TunnelServiceRequest::ChallengeCode(code, self.params.clone()),
                CONNECT_TIMEOUT,
            )
            .await;
        match response {
            Ok(TunnelServiceResponse::Ok) => self.do_status().await,
            Ok(TunnelServiceResponse::Error(e)) => Err(anyhow!(e)),
            Ok(_) => Err(anyhow!("Invalid response!")),
            Err(e) => Err(e),
        }
    }

    async fn do_disconnect(&mut self) -> anyhow::Result<ConnectionStatus> {
        self.send_receive(TunnelServiceRequest::Disconnect, RECV_TIMEOUT)
            .await?;
        self.do_status().await
    }

    async fn send_receive(
        &self,
        request: TunnelServiceRequest,
        timeout: Duration,
    ) -> anyhow::Result<TunnelServiceResponse> {
        let udp = tokio::net::UdpSocket::bind("127.0.0.1:0").await?;
        udp.connect(format!("127.0.0.1:{}", crate::server::LISTEN_PORT)).await?;

        let data = serde_json::to_vec(&request)?;

        let result = udp.send_receive(&data, timeout).await?;

        Ok(serde_json::from_slice(&result)?)
    }

    async fn fill_pwd_prompts(&mut self) -> anyhow::Result<()> {
        self.pwd_prompts
            .replace(server_info::get_pwd_prompts(&self.params).await.unwrap_or_default());
        Ok(())
    }

    async fn do_info(&self) -> anyhow::Result<ConnectionStatus> {
        let client = CccHttpClient::new(Arc::new(self.params.clone()), None);
        let info = client.get_server_info().await?;

        crate::util::print_login_options(&info);

        Ok(ConnectionStatus::default())
    }
}
