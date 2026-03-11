//! Raft network — HTTP-based RPC transport between cluster nodes.

use openraft::{
    BasicNode,
    error::{RPCError, RaftError, Unreachable, InstallSnapshotError},
    network::{RaftNetwork, RaftNetworkFactory, RPCOption},
    raft::{
        AppendEntriesRequest, AppendEntriesResponse,
        VoteRequest, VoteResponse,
        InstallSnapshotRequest, InstallSnapshotResponse,
    },
};

use super::StormTypeConfig;

/// Factory that creates HTTP(S) network connections to peer nodes.
pub struct HttpNetworkFactory {
    client: reqwest::Client,
    scheme: String,
}

impl Default for HttpNetworkFactory {
    fn default() -> Self {
        HttpNetworkFactory {
            client: reqwest::Client::new(),
            scheme: "http".to_string(),
        }
    }
}

impl HttpNetworkFactory {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn with_tls(client: reqwest::Client) -> Self {
        HttpNetworkFactory {
            client,
            scheme: "https".to_string(),
        }
    }
}

impl RaftNetworkFactory<StormTypeConfig> for HttpNetworkFactory {
    type Network = HttpNetwork;

    async fn new_client(&mut self, _target: u64, node: &BasicNode) -> Self::Network {
        HttpNetwork {
            addr: node.addr.clone(),
            client: self.client.clone(),
            scheme: self.scheme.clone(),
        }
    }
}

/// HTTP(S) network connection to a single peer node.
pub struct HttpNetwork {
    addr: String,
    client: reqwest::Client,
    scheme: String,
}

impl HttpNetwork {
    fn url(&self, path: &str) -> String {
        format!("{}://{}{}", self.scheme, self.addr, path)
    }
}

impl RaftNetwork<StormTypeConfig> for HttpNetwork {
    async fn append_entries(
        &mut self,
        rpc: AppendEntriesRequest<StormTypeConfig>,
        _option: RPCOption,
    ) -> Result<AppendEntriesResponse<u64>, RPCError<u64, BasicNode, RaftError<u64>>> {
        let resp = self.client
            .post(self.url("/raft/append"))
            .json(&rpc)
            .send()
            .await
            .map_err(|e| RPCError::Unreachable(Unreachable::new(&e)))?;

        let body = resp.json().await
            .map_err(|e| RPCError::Unreachable(Unreachable::new(&e)))?;
        Ok(body)
    }

    async fn install_snapshot(
        &mut self,
        rpc: InstallSnapshotRequest<StormTypeConfig>,
        _option: RPCOption,
    ) -> Result<
        InstallSnapshotResponse<u64>,
        RPCError<u64, BasicNode, RaftError<u64, InstallSnapshotError>>,
    > {
        let resp = self.client
            .post(self.url("/raft/snapshot"))
            .json(&rpc)
            .send()
            .await
            .map_err(|e| RPCError::Unreachable(Unreachable::new(&e)))?;

        let body = resp.json().await
            .map_err(|e| RPCError::Unreachable(Unreachable::new(&e)))?;
        Ok(body)
    }

    async fn vote(
        &mut self,
        rpc: VoteRequest<u64>,
        _option: RPCOption,
    ) -> Result<VoteResponse<u64>, RPCError<u64, BasicNode, RaftError<u64>>> {
        let resp = self.client
            .post(self.url("/raft/vote"))
            .json(&rpc)
            .send()
            .await
            .map_err(|e| RPCError::Unreachable(Unreachable::new(&e)))?;

        let body = resp.json().await
            .map_err(|e| RPCError::Unreachable(Unreachable::new(&e)))?;
        Ok(body)
    }
}
