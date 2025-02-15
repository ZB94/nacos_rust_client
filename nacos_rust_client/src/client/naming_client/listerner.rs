
use crate::client::naming_client::NamingUtils;
use crate::client::utils::Utils;
use crate::client::naming_client::UdpDataCmd;
use crate::client::naming_client::Duration;
use crate::client::now_millis;
use crate::client::naming_client::QueryListResult;
use crate::client::naming_client::UdpWorker;
use crate::client::naming_client::InnerNamingRequestClient;
use inner_mem_cache::TimeoutSet;
use std::collections::HashMap;
use crate::client::naming_client::ServiceInstanceKey;
use std::sync::Arc;
use actix::prelude::*;

use super::udp_actor::{InitLocalAddr,UdpWorkerCmd};
use super::{Instance, QueryInstanceListParams};

type InstanceListenerValue= Vec<Arc<Instance>>;
pub trait InstanceListener {
    fn get_key(&self) -> ServiceInstanceKey;
    fn change(&self,key:&ServiceInstanceKey,value:&InstanceListenerValue,add_list:&InstanceListenerValue,remove_list:&InstanceListenerValue) -> ();
}

#[derive(Clone)]
pub struct InstanceDefaultListener {
    key:ServiceInstanceKey,
    pub content:Arc<std::sync::RwLock<Option<Arc<Vec<Arc<Instance>>>>>>,
    pub callback:Option<Arc<Fn(Arc<InstanceListenerValue>,InstanceListenerValue,InstanceListenerValue)-> () +Send+Sync>>,
}

impl InstanceDefaultListener{
    pub fn new( key:ServiceInstanceKey,callback:Option<Arc<Fn(Arc<InstanceListenerValue>,InstanceListenerValue,InstanceListenerValue)-> () +Send+Sync>>) -> Self {
        Self{
            key,
            content: Default::default(),
            callback,
        }
    }

    pub fn get_content(&self) -> Arc<Vec<Arc<Instance>>> {
        match self.content.read().unwrap().as_ref() {
            Some(c) => c.clone(),
            _ => Default::default()
        }
    }

    fn set_value(content:Arc<std::sync::RwLock<Option<Arc<Vec<Arc<Instance>>>>>>,value:Vec<Arc<Instance>>){
        let mut r = content.write().unwrap();
        *r = Some(Arc::new(value));
    }

}

impl InstanceListener for InstanceDefaultListener {
    
    fn get_key(&self) -> ServiceInstanceKey { 
        self.key.clone()
    }

    fn change(&self,key:&ServiceInstanceKey,value:&Vec<Arc<Instance>>,add:&Vec<Arc<Instance>>,remove:&Vec<Arc<Instance>>) -> () {
        log::debug!("InstanceDefaultListener change,key{:?},valid count:{},add count:{},remove count:{}",key,value.len(),add.len(),remove.len());
        let content = self.content.clone();
        if value.len() > 0 {
            Self::set_value(content, value.clone());
            if let Some(callback) = &self.callback {
                callback(self.get_content(),add.clone(),remove.clone());
            }
        }
    }
}


struct ListenerValue{
    pub listener_key:ServiceInstanceKey,
    pub listener: Box<dyn InstanceListener+Send>,
    pub id:u64,
}

impl ListenerValue{
    fn new(listener_key:ServiceInstanceKey,listener:Box<dyn InstanceListener+Send>,id:u64) -> Self{
        Self{
            listener_key,
            listener,
            id,
        }
    }
}

#[derive(Debug,Default,Clone)]
struct InstancesWrap{
    instances: Vec<Arc<Instance>>,
    params:QueryInstanceListParams,
    last_sign:String,
    next_time:u64,
}


pub struct InnerNamingListener {
    namespace_id:String,
    //group@@servicename
    listeners:HashMap<String,Vec<ListenerValue>>,
    instances:HashMap<String,InstancesWrap>,
    timeout_set:TimeoutSet<String>,
    request_client:InnerNamingRequestClient,
    period: u64,
    client_ip:String,
    udp_port:u16,
    udp_addr:Addr<UdpWorker>,
}

impl InnerNamingListener {
    pub fn new(namespace_id:&str,client_ip:&str,udp_port:u16,request_client:InnerNamingRequestClient,udp_addr:Addr<UdpWorker>) -> Self{
        Self{
            namespace_id:namespace_id.to_owned(),
            listeners: Default::default(),
            instances: Default::default(),
            timeout_set: Default::default(),
            request_client,
            period:3000,
            client_ip:client_ip.to_owned(),
            udp_port:udp_port,
            udp_addr,
        }
    }

    pub fn query_instance(&self,key:String,ctx:&mut actix::Context<Self>) {
        let client = self.request_client.clone();
        if let Some(instance_warp) = self.instances.get(&key) {
            let params= instance_warp.params.clone();
            async move{
                (key,client.get_instance_list(&params).await)
            }.into_actor(self)
            .map(|(key,res),act,ctx|{
                match res {
                    Ok(result) => {
                    act.update_instances_and_notify(key, result);
                    },
                    Err(e) =>{
                        log::error!("get_instance_list error:{}",e);
                    },
                };
            })
            .spawn(ctx);
        }
    }

    fn update_instances_and_notify(&mut self,key:String,result:QueryListResult) -> anyhow::Result<()> {
        if let Some(cache_millis) = result.cacheMillis {
            self.period = cache_millis;
        }
        let mut is_notify=false;
        let mut old_instance_map = HashMap::new();
        if let Some(instance_warp) = self.instances.get_mut(&key) {
            let checksum = result.checksum.unwrap_or("".to_owned());
            if instance_warp.last_sign != checksum || instance_warp.last_sign.len()==0 {
                instance_warp.last_sign = checksum;
                if let Some(hosts) = result.hosts {
                    for e in &instance_warp.instances {
                        old_instance_map.insert(format!("{}:{}",e.ip,e.port), e.clone());
                    }
                    instance_warp.instances = hosts.into_iter()
                        .map(|e| Arc::new(e.to_instance()))
                        .filter(|e|e.weight>0.001f32)
                        .collect();
                    is_notify=true;
                }
            }
            let current_time = now_millis();
            instance_warp.next_time = current_time+self.period;
        }
        if is_notify {
            if let Some(instance_warp) = self.instances.get(&key) {
                let mut add_list = vec![];
                for item in &instance_warp.instances {
                    let key = format!("{}:{}",item.ip,item.port);
                    if old_instance_map.remove(&key).is_none() {
                        add_list.push(item.clone());
                    }
                }
                let remove_list:Vec<Arc<Instance>> = old_instance_map.into_iter().map(|(k,v)| {v}).collect();
                self.notify_listener(key, &instance_warp.instances,add_list,remove_list);
            }
        }
        Ok(())
    }

    fn notify_listener(&self,key_str:String,instances:&Vec<Arc<Instance>>,add_list:Vec<Arc<Instance>>,remove_list:Vec<Arc<Instance>>) {
        if add_list.len()==0 && remove_list.len()==0 {
            return;
        }
        let key =ServiceInstanceKey::from_str(&key_str); 
        if let Some(list) = self.listeners.get(&key_str) {
            for item in list {
                item.listener.change(&key, instances,&add_list,&remove_list);
            }
        }
    }

    fn filter_instances(&mut self,params:&QueryInstanceListParams,ctx:&mut actix::Context<Self>) -> Option<Vec<Arc<Instance>>>{
        let key = params.get_key();
        if let Some(instance_warp) = self.instances.get(&key) {
            let mut list = vec![];
            for item in &instance_warp.instances {
                if params.healthy_only && !item.healthy {
                    continue;
                }
                if let Some(clusters) = &params.clusters {
                    let name = &item.cluster_name;
                    if !clusters.contains(name) {
                        continue;
                    }
                }
                list.push(item.clone());
            }
            return Some(list);
            //if list.len()> 0 {
            //    return Some(list);
            //}
        }
        else{
            let addr = ctx.address();
            addr.do_send(NamingListenerCmd::AddHeartbeat(ServiceInstanceKey::from_str(&key)));
        }
        None
    }

    pub fn hb(&self,ctx:&mut actix::Context<Self>) {
        ctx.run_later(Duration::new(1,0), |act,ctx|{
            let current_time = now_millis();
            let addr = ctx.address();
            for key in act.timeout_set.timeout(current_time){
                addr.do_send(NamingListenerCmd::Heartbeat(key,current_time));
            }
            act.hb(ctx);
        });
    }

    pub fn init_udp_info(&self,ctx:&mut actix::Context<Self>) {
        self.udp_addr.do_send(UdpWorkerCmd::SetListenerAddr(ctx.address()));
        if self.udp_port ==0 {
            self.udp_addr.do_send(UdpWorkerCmd::QueryUdpPort);
        }
    }
}

impl Actor for InnerNamingListener {
    type Context = Context<Self>;

    fn started(&mut self,ctx: &mut Self::Context) {
        log::info!(" InnerNamingListener started");
        self.init_udp_info(ctx);
        self.hb(ctx);
    }
}

#[derive(Message)]
#[rtype(result = "Result<(),std::io::Error>")]
pub enum NamingListenerCmd {
    Add(ServiceInstanceKey,u64,Box<InstanceListener+Send+'static>),
    Remove(ServiceInstanceKey,u64),
    AddHeartbeat(ServiceInstanceKey),
    Heartbeat(String,u64),
    Close,
}

impl Handler<NamingListenerCmd> for InnerNamingListener {
    type Result = Result<(),std::io::Error>;

    fn handle(&mut self,msg:NamingListenerCmd,ctx:&mut Context<Self>) -> Self::Result  {
        match msg {
            NamingListenerCmd::Add(key,id,listener) => {
                let key_str = key.get_key();
                //如果已经存在，则直接触发一次
                if let Some(instance_wrap) = self.instances.get(&key_str) {
                    if instance_wrap.instances.len() > 0{
                        listener.change(&key, &instance_wrap.instances,&instance_wrap.instances,&vec![]);
                    }
                }
                let listener_value = ListenerValue::new(key.clone(),listener,id);
                if let Some(list) = self.listeners.get_mut(&key_str) {
                    list.push(listener_value);
                }
                else{
                    self.listeners.insert(key_str.clone(), vec![listener_value]);
                    let addr = ctx.address();
                    addr.do_send(NamingListenerCmd::AddHeartbeat(key));
                }
            },
            NamingListenerCmd::AddHeartbeat(key) => {
                let key_str = key.get_key();
                if let Some(_) = self.instances.get(&key_str) {
                    return Ok(());
                }
                else{
                    //println!("======== AddHeartbeat ,key:{}",&key_str);
                    let current_time = now_millis();
                    let mut instances=InstancesWrap::default();
                    instances.params.group_name=key.group_name;
                    instances.params.service_name=key.service_name;
                    instances.params.namespace_id=self.namespace_id.to_owned();
                    instances.params.healthy_only=false;
                    instances.params.client_ip=Some(self.client_ip.clone());
                    instances.params.udp_port = Some(self.udp_port);
                    instances.next_time=current_time;
                    self.instances.insert(key_str.clone(), instances);
                    //self.timeout_set.add(0u64,key_str);
                    let addr = ctx.address();
                    addr.do_send(NamingListenerCmd::Heartbeat(key_str,current_time));
                }
            },
            NamingListenerCmd::Remove(key,id) => {
                let key_str = key.get_key();
                if let Some(list) = self.listeners.get_mut(&key_str) {
                    let mut indexs = Vec::new();
                    for i in 0..list.len() {
                        if let Some(item) = list.get(i){
                            if item.id==id {
                                indexs.push(i);
                            }
                        }
                    }
                    for i in indexs.iter().rev() {
                        list.remove(*i);
                    }
                }
            },
            NamingListenerCmd::Heartbeat(key, time) => {
                let mut is_query=false;
                if let Some(instance_warp) = self.instances.get_mut(&key) {
                    if instance_warp.next_time> time {
                        self.timeout_set.add(instance_warp.next_time,key.clone());
                        return Ok(())
                    }
                    is_query=true;
                    let current_time = now_millis();
                    instance_warp.next_time = current_time+self.period;
                    self.timeout_set.add(instance_warp.next_time,key.clone());
                }
                if is_query {
                    self.query_instance(key, ctx);
                }
            },
            NamingListenerCmd::Close => {
                self.udp_addr.do_send(UdpWorkerCmd::Close);
                log::info!("InnerNamingListener close");
                ctx.stop();
            },
        };
        Ok(())
    }
}

impl Handler<UdpDataCmd> for InnerNamingListener {
    type Result = Result<(),std::io::Error>;
    fn handle(&mut self,msg:UdpDataCmd,ctx: &mut Context<Self>) -> Self::Result {
        let data = match Utils::gz_decode(&msg.data){
            Some(data) => data,
            None => msg.data,
        };
        let map:HashMap<String,String> = serde_json::from_slice(&data).unwrap_or_default();
        if let Some(str_data) = map.get("data") {
            let result:QueryListResult=serde_json::from_str(str_data)?;
            let ref_time  = result.lastRefTime.clone().unwrap_or_default();
            let key = result.name.clone().unwrap_or_default();
            //send to client
            let mut map = HashMap::new();
            map.insert("type", "push-ack".to_owned());
            map.insert("lastRefTime",ref_time.to_string());
            map.insert("data","".to_owned());
            let ack = serde_json::to_string(&map).unwrap();
            let send_msg = UdpDataCmd{
                data:ack.as_bytes().to_vec(),
                target_addr:msg.target_addr,
            };
            self.udp_addr.do_send(send_msg);
            //update
            self.update_instances_and_notify(key,result);
        }
        Ok(())
    }
}

impl Handler<InitLocalAddr> for InnerNamingListener {
    type Result = Result<(),std::io::Error>;
    fn handle(&mut self, msg: InitLocalAddr, ctx: &mut Self::Context) -> Self::Result {
        log::info!("InnerNamingListener init udp port by InitLocalAddr:{},oldport:{}",&msg.port,&self.udp_port);
        self.udp_port = msg.port;
        Ok(())
    }
}

type ListenerSenderType = tokio::sync::oneshot::Sender<NamingQueryResult>;
type ListenerReceiverType = tokio::sync::oneshot::Receiver<NamingQueryResult>;

#[derive(Message)]
#[rtype(result = "Result<NamingQueryResult,std::io::Error>")]
pub enum NamingQueryCmd{
    QueryList(QueryInstanceListParams,ListenerSenderType),
    Select(QueryInstanceListParams,ListenerSenderType),
}

pub enum NamingQueryResult {
    None,
    One(Arc<Instance>),
    List(Vec<Arc<Instance>>),
}

impl Handler<NamingQueryCmd> for InnerNamingListener {
    type Result = Result<NamingQueryResult,std::io::Error>;
    fn handle(&mut self,msg:NamingQueryCmd,ctx:&mut Context<Self>) -> Self::Result  {
        match msg {
            NamingQueryCmd::QueryList(param,sender) => {
                if let Some(list) = self.filter_instances(&param,ctx) {
                    sender.send(NamingQueryResult::List(list));
                }
                else{
                    let request_client = self.request_client.clone();
                    async move {
                        (request_client.get_instance_list(&param).await,sender,param)
                    }
                    .into_actor(self)
                    .map(|(res,sender,param),act,ctx|{
                        match res {
                            Ok(list_result) => {
                                let key = param.get_key();
                                act.update_instances_and_notify(key, list_result);
                                if let Some(list) = act.filter_instances(&param,ctx) {
                                    sender.send(NamingQueryResult::List(list));
                                    return;
                                }
                            },
                            Err(_) => {},
                        }
                        sender.send(NamingQueryResult::None);
                    })
                    .spawn(ctx);
                }
            },
            NamingQueryCmd::Select(param,sender) => {
                if let Some(list) = self.filter_instances(&param,ctx) {
                    let index = NamingUtils::select_by_weight_fn(&list, |e| (e.weight*1000f32) as u64); 
                    if let Some(e) = list.get(index) {
                        sender.send(NamingQueryResult::One(e.clone()));
                    }
                    else{
                        sender.send(NamingQueryResult::None);
                    }
                }
                else{
                    let request_client = self.request_client.clone();
                    async move {
                        (request_client.get_instance_list(&param).await,sender,param)
                    }
                    .into_actor(self)
                    .map(|(res,sender,param),act,ctx|{
                        match res {
                            Ok(list_result) => {
                                let key = param.get_key();
                                act.update_instances_and_notify(key, list_result);
                                if let Some(list) = act.filter_instances(&param,ctx) {
                                    let index = NamingUtils::select_by_weight_fn(&list, |e| (e.weight*1000f32) as u64); 
                                    if let Some(e) = list.get(index) {
                                        sender.send(NamingQueryResult::One(e.clone()));
                                        return;
                                    }
                                }
                            },
                            Err(_) => {},
                        }
                        sender.send(NamingQueryResult::None);
                    })
                    .spawn(ctx); 
                }
            },
        }
        Ok(NamingQueryResult::None)
    }
}

