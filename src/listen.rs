// 运行期切换监听地址：新地址先绑定成功再关闭旧监听；
// 同端口换绑定地址时先释放旧端口，失败则回滚。

use tokio::net::TcpListener;
use tokio::task::JoinHandle;

use axum::Router;

pub struct ListenManager {
    addr: String,
    task: Option<JoinHandle<()>>,
}

fn split_port(addr: &str) -> Option<String> {
    addr.rsplit(':').next().map(|s| s.to_string())
}

fn spawn_serve(ln: TcpListener, app: Router) -> JoinHandle<()> {
    tokio::spawn(async move {
        let svc = app.into_make_service_with_connect_info::<std::net::SocketAddr>();
        let _ = axum::serve(ln, svc).await;
    })
}

impl ListenManager {
    pub async fn new(addr: &str, app: Router) -> Result<ListenManager, String> {
        let ln = TcpListener::bind(addr)
            .await
            .map_err(|e| format!("listen {addr}: {e}"))?;
        Ok(ListenManager {
            addr: addr.to_string(),
            task: Some(spawn_serve(ln, app)),
        })
    }

    #[allow(dead_code)]
    pub fn addr(&self) -> &str {
        &self.addr
    }

    /// 切换监听地址。语义与 Go 版一致：
    /// 1) 新地址能直接绑定 → 先起新监听再关旧的；
    /// 2) 同端口换 host（通配与具体地址互斥）→ 先关旧的再绑新的，失败则回滚旧地址。
    pub async fn change(&mut self, addr: &str, app: Router) -> Result<(), String> {
        if addr == self.addr {
            return Ok(());
        }
        match TcpListener::bind(addr).await {
            Ok(ln) => {
                self.swap(ln, addr, app).await;
                return Ok(());
            }
            Err(e) => {
                let same = match (split_port(&self.addr), split_port(addr)) {
                    (Some(a), Some(b)) => a == b,
                    _ => false,
                };
                if !same {
                    return Err(e.to_string());
                }
            }
        }
        // 同端口：先关旧监听再重试
        self.stop_task().await;
        match TcpListener::bind(addr).await {
            Ok(ln) => {
                self.swap(ln, addr, app).await;
                Ok(())
            }
            Err(retry_err) => {
                // 回滚旧地址
                match TcpListener::bind(&self.addr).await {
                    Ok(ln) => self.task = Some(spawn_serve(ln, app)),
                    Err(back_err) => {
                        eprintln!("监听地址无法恢复 {}: {back_err}", self.addr);
                    }
                }
                Err(retry_err.to_string())
            }
        }
    }

    async fn swap(&mut self, ln: TcpListener, addr: &str, app: Router) {
        let old_addr = self.addr.clone();
        self.stop_task().await;
        self.task = Some(spawn_serve(ln, app));
        self.addr = addr.to_string();
        eprintln!("监听地址已切换: {old_addr} -> {addr}");
    }

    async fn stop_task(&mut self) {
        if let Some(t) = self.task.take() {
            t.abort();
            let _ = t.await;
        }
    }

    pub async fn shutdown(&mut self) {
        self.stop_task().await;
    }
}
