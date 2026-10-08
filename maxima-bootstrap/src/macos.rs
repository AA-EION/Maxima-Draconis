use cacao::appkit::{App, AppDelegate};
use url::Url;

use crate::{handle_launch_args, run};

pub struct MaximaBootstrapApp {
    rt: tokio::runtime::Handle,
}

impl MaximaBootstrapApp {
    pub fn new(rt: tokio::runtime::Handle) -> Self {
        Self { rt }
    }
}

impl AppDelegate for MaximaBootstrapApp {
    fn did_finish_launching(&self) {
        self.rt.spawn(async {
            // Terminate on any completed outcome — Ok(false) means "no args,
            // stay alive for open_urls"; Err must also terminate, otherwise a
            // failed game launch leaves this process (and the parent's
            // playing-state tracking) hanging forever.
            match handle_launch_args().await {
                Ok(false) => {}
                _ => App::terminate(),
            }
        });
    }

    fn open_urls(&self, urls: Vec<Url>) {
        self.rt.spawn(async move {
            let _ = run(&urls.iter().map(|u| u.to_string()).collect::<Vec<String>>()).await;
            App::terminate();
        });
    }
}
