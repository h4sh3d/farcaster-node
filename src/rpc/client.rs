// LNP Node: node running lightning network protocol and generalized lightning
// channels.
// Written in 2020 by
//     Dr. Maxim Orlovsky <orlovsky@pandoracore.com>
//
// To the extent possible under law, the author(s) have dedicated all
// copyright and related and neighboring rights to this software to
// the public domain worldwide. This software is distributed without
// any warranty.
//
// You should have received a copy of the MIT License
// along with this software.
// If not, see <https://opensource.org/licenses/MIT>.

use crate::service::Endpoints;
use std::convert::TryInto;
use std::thread::sleep;
use std::time::Duration;

use internet2::ZmqType;
use microservices::esb;

use crate::rpc::request::OptionDetails;
use crate::rpc::{Request, ServiceBus};
use crate::service::ServiceConfig;
use crate::{Error, LogStyle, ServiceId};

#[repr(C)]
pub struct Client {
    identity: ServiceId,
    response_queue: std::collections::VecDeque<Request>,
    esb: esb::Controller<ServiceBus, Request, Handler>,
}

impl Client {
    pub fn with(config: ServiceConfig) -> Result<Self, Error> {
        debug!("Setting up RPC client...");
        let identity = ServiceId::client();
        let bus_config = esb::BusConfig::with_locator(
            config
                .ctl_endpoint
                .try_into()
                .expect("Only ZMQ RPC is currently supported"),
            Some(ServiceId::router()),
        );
        let esb = esb::Controller::with(
            map! {
                ServiceBus::Ctl => bus_config
            },
            Handler {
                identity: identity.clone(),
            },
            ZmqType::RouterConnect,
        )?;

        // We have to sleep in order for ZMQ to bootstrap
        sleep(Duration::from_secs_f32(0.1));

        Ok(Self {
            identity,
            response_queue: empty!(),
            esb,
        })
    }

    pub fn identity(&self) -> ServiceId {
        self.identity.clone()
    }

    pub fn request(&mut self, daemon: ServiceId, req: Request) -> Result<(), Error> {
        debug!("Executing {}", req);
        self.esb.send_to(ServiceBus::Ctl, daemon, req)?;
        Ok(())
    }

    pub fn response(&mut self) -> Result<Request, Error> {
        if self.response_queue.is_empty() {
            for (_, _, rep) in self.esb.recv_poll()? {
                self.response_queue.push_back(rep);
            }
        }
        Ok(self
            .response_queue
            .pop_front()
            .expect("We always have at least one element"))
    }

    pub fn report_failure(&mut self) -> Result<Request, Error> {
        match self.response()? {
            Request::Failure(fail) =>
            // // Errors reported on swap-cli binary
            // eprintln!(
            //     "{}: {}",
            //     "Request failure".err(),
            //     fail.err_details()
            // );
            {
                Err(Error::from(fail))
            }
            resp => Ok(resp),
        }
    }

    pub fn report_response(&mut self) -> Result<(), Error> {
        let resp = self.report_failure()?;
        println!("{:#}", resp);
        Ok(())
    }

    pub fn report_progress(&mut self) -> Result<usize, Error> {
        let mut counter = 0;
        let mut finished = false;
        while !finished {
            finished = true;
            counter += 1;
            match self.report_failure()? {
                // Failure is already covered by `report_response()`
                Request::Progress(info) => {
                    println!("{}", info.green_bold());
                    finished = false;
                }
                Request::Success(OptionDetails(Some(info))) => {
                    println!("{}{}", "Success: ".bright_green_bold(), info);
                }
                Request::Success(OptionDetails(None)) => {
                    println!("{}", "Success".bright_green_bold());
                }
                other => {
                    eprintln!("{}: {}", "Unexpected report".err(), other.err_details());
                    return Err(Error::Other(s!("Unexpected server response")));
                }
            }
        }
        Ok(counter)
    }
}

pub struct Handler {
    identity: ServiceId,
}

impl esb::Handler<ServiceBus> for Handler {
    type Request = Request;
    type Error = Error;

    fn identity(&self) -> ServiceId {
        self.identity.clone()
    }

    fn handle(
        &mut self,
        _endpoints: &mut Endpoints,
        _bus: ServiceBus,
        _addr: ServiceId,
        _request: Request,
    ) -> Result<(), Error> {
        // Cli does not receive replies for now
        Ok(())
    }

    fn handle_err(&mut self, _: &mut Endpoints, err: esb::Error<ServiceId>) -> Result<(), Error> {
        // We simply propagate the error since it's already being reported
        Err(Error::Esb(err))
    }
}
