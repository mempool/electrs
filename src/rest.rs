use config::Config;

use std::sync::Arc;

use hyper::{Body, Response, Server, Method};
use hyper::service::service_fn_ok;
use hyper::rt::{self, Future};
use std::thread;
use query::Query;
use hyper::Request;

pub fn run_server(_config: &Config, query: Arc<Query>) {

    let addr = ([127, 0, 0, 1], 3000).into();  // TODO take from config
    info!("REST server running on {}", addr);

    let new_service = move || {

        let query = query.clone();

        //TODO handle errors using service_fn
        service_fn_ok(move |req: Request<Body>| {

            // TODO it looks hyper does not have routing :(
            let path: Vec<&str> = req.uri().path().split('/').filter(|el| !el.is_empty()).collect();
            info!("path {:?}", path);

            match (req.method(), path.get(0), path.get(1), path.get(2)) {
                (&Method::GET, Some(&"blocks"), None, None) => {
                    Response::new(Body::from(format!("1 Request{:?}", query.get_best_header().unwrap().header() )))
                },
                (&Method::GET, Some(&"block"), Some(str), None) => {
                    Response::new(Body::from(format!("block {}", str )))
                },
                (&Method::GET, Some(&"block"), Some(str), Some(&"txs")) => {
                    Response::new(Body::from(format!("block with txs {}", str )))
                },
                (&Method::GET, Some(&"tx"), Some(str), None) => {
                    Response::new(Body::from(format!("tx {}", str )))
                },
                _ => {
                    Response::new(Body::from("hello"))
                }
            }

        })
    };

    let server = Server::bind(&addr)
        .serve(new_service)
        .map_err(|e| eprintln!("server error: {}", e));


    thread::spawn(move || {
        rt::run(server);
    });

}

#[cfg(test)]
mod tests {
    #[test]
    fn test_fakestore() {
        let x = "a b c d  as asfas ";
        let y : Vec<&str> = x.split(' ').collect();
        println!("{:?}", y);
        let y : Vec<&str> = x.split(' ').filter(|el| !el.is_empty() ).collect();
        println!("{:?}", y);
    }
}
