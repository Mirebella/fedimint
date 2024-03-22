use std::ops::Deref as _;
use std::sync::Arc;

use anyhow::Result;
use fedimint_core::task::jit::{JitTry, JitTryAnyhow};
use fedimint_logging::LOG_DEVIMINT;
use tracing::{debug, info};

use crate::external::{Bitcoind, Electrs, Esplora, Lightningd, Lnd};
use crate::federation::{Client, Federation};
use crate::gatewayd::Gatewayd;
use crate::util::ProcessManager;
use crate::{cmd, open_channel, LightningNode};

#[derive(Clone)]
pub struct DevFed {
    pub bitcoind: Bitcoind,
    pub cln: Lightningd,
    pub lnd: Lnd,
    pub fed: Federation,
    pub gw_cln: Gatewayd,
    pub gw_lnd: Gatewayd,
    pub electrs: Electrs,
    pub esplora: Esplora,
}

pub async fn dev_fed(process_mgr: &ProcessManager) -> Result<DevFed> {
    let fed_size = process_mgr.globals.FM_FED_SIZE;
    let offline_nodes = process_mgr.globals.FM_OFFLINE_NODES;
    anyhow::ensure!(
        fed_size > 3 * offline_nodes,
        "too many offline nodes ({offline_nodes}) to reach consensus"
    );

    let start_time = fedimint_core::time::now();
    info!("Starting dev federation");
    let bitcoind = Bitcoind::new(process_mgr).await?;
    let ((cln, lnd, gw_cln, gw_lnd), electrs, esplora, mut fed) = tokio::try_join!(
        async {
            debug!(target: LOG_DEVIMINT, "Starting LN nodes");
            let (cln, lnd) = tokio::try_join!(
                Lightningd::new(process_mgr, bitcoind.clone()),
                Lnd::new(process_mgr, bitcoind.clone())
            )?;
            debug!(target: LOG_DEVIMINT, "Starting LN gateways & opening LN channel");
            let (gw_cln, gw_lnd, _) = tokio::try_join!(
                Gatewayd::new(process_mgr, LightningNode::Cln(cln.clone())),
                Gatewayd::new(process_mgr, LightningNode::Lnd(lnd.clone())),
                open_channel(process_mgr, &bitcoind, &cln, &lnd),
            )?;
            debug!(target: LOG_DEVIMINT, "LN gateways ready");
            Ok((cln, lnd, gw_cln, gw_lnd))
        },
        Electrs::new(process_mgr, bitcoind.clone()),
        Esplora::new(process_mgr, bitcoind.clone()),
        Federation::new(process_mgr, bitcoind.clone(), fed_size),
    )?;

    info!(target: LOG_DEVIMINT, "Federation and gateways started");

    std::env::set_var("FM_GWID_CLN", gw_cln.gateway_id().await?);
    std::env::set_var("FM_GWID_LND", gw_lnd.gateway_id().await?);
    info!(target: LOG_DEVIMINT, "Setup gateway environment variables");

    tokio::try_join!(gw_cln.connect_fed(&fed), gw_lnd.connect_fed(&fed), async {
        info!(target: LOG_DEVIMINT, "Joining federation with the main client");
        cmd!(fed.internal_client(), "join-federation", fed.invite_code()?)
            .run()
            .await?;
        debug!(target: LOG_DEVIMINT, "Generating first epoch");
        fed.mine_then_wait_blocks_sync(10).await?;
        Ok(())
    })?;

    // Initialize fedimint-cli
    fed.await_gateways_registered().await?;

    // Create a degraded federation if there are offline nodes
    fed.degrade_federation(process_mgr).await?;

    info!(
        target: LOG_DEVIMINT,
        fed_size,
        offline_nodes,
        elapsed_ms = %start_time.elapsed()?.as_millis(),
        "Dev federation ready",
    );

    Ok(DevFed {
        bitcoind,
        cln,
        lnd,
        fed,
        gw_cln,
        gw_lnd,
        electrs,
        esplora,
    })
}

type JitArc<T> = JitTryAnyhow<Arc<T>>;

#[derive(Clone)]
pub struct DevJitFed {
    bitcoind: JitArc<Bitcoind>,
    cln: JitArc<Lightningd>,
    lnd: JitArc<Lnd>,
    fed: JitArc<Federation>,
    gw_cln: JitArc<Gatewayd>,
    gw_lnd: JitArc<Gatewayd>,
    electrs: JitArc<Electrs>,
    esplora: JitArc<Esplora>,
    start_time: std::time::SystemTime,
    gw_cln_registered: JitArc<()>,
    gw_lnd_registered: JitArc<()>,
    fed_client_joined: JitArc<()>,
    fed_epoch_generated: JitArc<()>,
    channel_opened: JitArc<()>,
}

impl DevJitFed {
    pub fn new(process_mgr: &ProcessManager) -> Result<DevJitFed> {
        let fed_size = process_mgr.globals.FM_FED_SIZE;
        let offline_nodes = process_mgr.globals.FM_OFFLINE_NODES;
        anyhow::ensure!(
            fed_size > 3 * offline_nodes,
            "too many offline nodes ({offline_nodes}) to reach consensus"
        );
        let start_time = fedimint_core::time::now();

        info!("Starting dev federation");

        let bitcoind = JitTry::new_try({
            let process_mgr = process_mgr.to_owned();
            move || async move { Ok(Arc::new(Bitcoind::new(&process_mgr).await?)) }
        });
        let cln = JitTry::new_try({
            let process_mgr = process_mgr.to_owned();
            let bitcoind = bitcoind.clone();
            move || async move {
                Ok(Arc::new(
                    Lightningd::new(&process_mgr, bitcoind.get_try().await?.deref().clone())
                        .await?,
                ))
            }
        });
        let lnd = JitTry::new_try({
            let process_mgr = process_mgr.to_owned();
            let bitcoind = bitcoind.clone();
            move || async move {
                Ok(Arc::new(
                    Lnd::new(&process_mgr, bitcoind.get_try().await?.deref().clone()).await?,
                ))
            }
        });
        let electrs = JitTryAnyhow::new_try({
            let process_mgr = process_mgr.to_owned();
            let bitcoind = bitcoind.clone();
            move || async move {
                let bitcoind = bitcoind.get_try().await?.deref().clone();
                Ok(Arc::new(Electrs::new(&process_mgr, bitcoind).await?))
            }
        });
        let esplora = JitTryAnyhow::new_try({
            let process_mgr = process_mgr.to_owned();
            let bitcoind = bitcoind.clone();
            move || async move {
                let bitcoind = bitcoind.get_try().await?.deref().clone();
                Ok(Arc::new(Esplora::new(&process_mgr, bitcoind).await?))
            }
        });

        let fed = JitTryAnyhow::new_try({
            let process_mgr = process_mgr.to_owned();
            let bitcoind = bitcoind.clone();
            move || async move {
                let bitcoind = bitcoind.get_try().await?.deref().clone();
                let mut fed = Federation::new(&process_mgr, bitcoind, fed_size).await?;

                // Create a degraded federation if there are offline nodes
                fed.degrade_federation(&process_mgr).await?;

                Ok(Arc::new(fed))
            }
        });

        let gw_cln = JitTryAnyhow::new_try({
            let process_mgr = process_mgr.to_owned();
            let cln = cln.clone();
            move || async move {
                let cln = cln.get_try().await?.deref().clone();
                Ok(Arc::new(
                    Gatewayd::new(&process_mgr, LightningNode::Cln(cln)).await?,
                ))
            }
        });
        let gw_cln_registered = JitTryAnyhow::new_try({
            let gw_cln = gw_cln.clone();
            let fed = fed.clone();
            move || async move {
                let gw_cln = gw_cln.get_try().await?.deref();
                let fed = fed.get_try().await?.deref();

                gw_cln.connect_fed(fed).await?;
                Ok(Arc::new(()))
            }
        });
        let gw_lnd = JitTryAnyhow::new_try({
            let process_mgr = process_mgr.to_owned();
            let lnd = lnd.clone();
            move || async move {
                let lnd = lnd.get_try().await?.deref().clone();
                Ok(Arc::new(
                    Gatewayd::new(&process_mgr, LightningNode::Lnd(lnd)).await?,
                ))
            }
        });
        let gw_lnd_registered = JitTryAnyhow::new_try({
            let gw_lnd = gw_lnd.clone();
            let fed = fed.clone();
            move || async move {
                let gw_lnd = gw_lnd.get_try().await?.deref();
                let fed = fed.get_try().await?.deref();

                gw_lnd.connect_fed(fed).await?;
                Ok(Arc::new(()))
            }
        });

        let channel_opened = JitTryAnyhow::new_try({
            let process_mgr = process_mgr.to_owned();
            let lnd = lnd.clone();
            let cln = cln.clone();
            let bitcoind = bitcoind.clone();
            move || async move {
                let bitcoind = bitcoind.get_try().await?.deref().clone();
                let lnd = lnd.get_try().await?.deref().clone();
                let cln = cln.get_try().await?.deref().clone();
                open_channel(&process_mgr, &bitcoind, &cln, &lnd).await?;
                Ok(Arc::new(()))
            }
        });

        let fed_epoch_generated = JitTryAnyhow::new_try({
            let fed = fed.clone();
            move || async move {
                let fed = fed.get_try().await?.deref().clone();
                fed.mine_then_wait_blocks_sync(10).await?;
                Ok(Arc::new(()))
            }
        });
        let fed_client_joined = JitTryAnyhow::new_try({
            let fed = fed.clone();
            move || async move {
                let fed = fed.get_try().await?.deref();
                cmd!(fed.internal_client(), "join-federation", fed.invite_code()?)
                    .run()
                    .await?;
                Ok(Arc::new(()))
            }
        });

        Ok(DevJitFed {
            bitcoind,
            cln,
            lnd,
            fed,
            gw_cln,
            gw_cln_registered,
            gw_lnd,
            gw_lnd_registered,
            electrs,
            esplora,
            channel_opened,
            fed_client_joined,
            fed_epoch_generated,
            start_time,
        })
    }

    pub async fn electrs(&self) -> anyhow::Result<&Electrs> {
        Ok(self.electrs.get_try().await?.deref())
    }
    pub async fn esplora(&self) -> anyhow::Result<&Esplora> {
        Ok(self.esplora.get_try().await?.deref())
    }
    pub async fn cln(&self) -> anyhow::Result<&Lightningd> {
        Ok(self.cln.get_try().await?.deref())
    }
    pub async fn lnd(&self) -> anyhow::Result<&Lnd> {
        Ok(self.lnd.get_try().await?.deref())
    }
    pub async fn gw_cln(&self) -> anyhow::Result<&Gatewayd> {
        Ok(self.gw_cln.get_try().await?.deref())
    }
    pub async fn gw_cln_registered(&self) -> anyhow::Result<&Gatewayd> {
        self.gw_cln_registered.get_try().await?;
        Ok(self.gw_cln.get_try().await?.deref())
    }
    pub async fn gw_lnd(&self) -> anyhow::Result<&Gatewayd> {
        Ok(self.gw_lnd.get_try().await?.deref())
    }
    pub async fn gw_lnd_registered(&self) -> anyhow::Result<&Gatewayd> {
        self.gw_lnd_registered.get_try().await?;
        Ok(self.gw_lnd.get_try().await?.deref())
    }
    pub async fn fed(&self) -> anyhow::Result<&Federation> {
        Ok(self.fed.get_try().await?.deref())
    }
    pub async fn bitcoind(&self) -> anyhow::Result<&Bitcoind> {
        Ok(self.bitcoind.get_try().await?.deref())
    }

    pub async fn client_registered(&self) -> anyhow::Result<Client> {
        self.fed_client_joined.get_try().await?;
        Ok(self.fed().await?.internal_client().clone())
    }

    pub async fn client_gw_registered(&self) -> anyhow::Result<Client> {
        self.fed_client_joined.get_try().await?;
        // Initialize fedimint-cli
        self.fed().await?.await_gateways_registered().await?;
        Ok(self.fed().await?.internal_client().clone())
    }

    pub async fn finalize(&self, process_mgr: &ProcessManager) -> anyhow::Result<()> {
        let fed_size = process_mgr.globals.FM_FED_SIZE;
        let offline_nodes = process_mgr.globals.FM_OFFLINE_NODES;
        anyhow::ensure!(
            fed_size > 3 * offline_nodes,
            "too many offline nodes ({offline_nodes}) to reach consensus"
        );

        std::env::set_var("FM_GWID_CLN", self.gw_cln().await?.gateway_id().await?);
        std::env::set_var("FM_GWID_LND", self.gw_lnd().await?.gateway_id().await?);
        info!(target: LOG_DEVIMINT, "Setup gateway environment variables");

        let _ = self.client_gw_registered().await?;
        let _ = self.channel_opened.get_try().await?;
        let _ = self.gw_cln_registered().await?;
        let _ = self.gw_lnd_registered().await?;
        let _ = self.cln().await?;
        let _ = self.lnd().await?;
        let _ = self.electrs().await?;
        let _ = self.esplora().await?;
        let _ = self.fed_epoch_generated.get_try().await?;

        info!(
            target: LOG_DEVIMINT,
            fed_size,
            offline_nodes,
            elapsed_ms = %self.start_time.elapsed()?.as_millis(),
            "Dev federation ready",
        );
        Ok(())
    }
}
