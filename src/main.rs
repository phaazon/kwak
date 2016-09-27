#![feature(custom_derive)]

extern crate hyper;
extern crate regex;
extern crate serde_json;

use hyper::client;
use hyper::header;
use hyper::mime;
use regex::Regex;

use serde_json::de;
use serde_json::ser;
use std::collections::BTreeMap;
use std::io::{BufRead, BufReader, Read, Write};
use std::net;
use std::path::Path;
use std::fs::File;

macro_rules! opt {
  ($option:expr) => {
    match $option {
      ::std::option::Option::Some(a) => a,
      ::std::option::Option::None => return None
    }
  }
}

struct IRCClient {
  stream: BufReader<net::TcpStream>,
  line_buf: String,
  nick: String,
  channel: String,
  tells: Tells
}

impl IRCClient {
  fn connect(addr: &str, port: u16, nick: &str, channel: &str) -> Self {
    let stream = BufReader::new(net::TcpStream::connect((addr, port)).unwrap());

    IRCClient {
      stream: stream,
      line_buf: String::with_capacity(1024),
      nick: nick.to_owned(),
      channel: channel.to_owned(),
      tells: read_tells("tells.json")
    }
  }

  fn save_tells(&mut self) {
    save_tells("tells.json", &self.tells);
  }

  fn read_line(&mut self) -> String {
    self.line_buf.clear();
    let _ = self.stream.read_line(&mut self.line_buf);
    self.line_buf.trim().to_owned()
  }

  fn write_line(&mut self, msg: &str) {
    let stream = self.stream.get_mut();
    let _ = stream.write(msg.as_bytes());
    let _ = stream.write("\n".as_bytes());
  }

  fn init(&mut self) {
    let nick = self.nick.clone();
    let chan = self.channel.clone();

    self.write_line("USER a b c :d");
    self.write_line(&format!("NICK {}", nick));
    self.write_line(&format!("JOIN {}", chan));
  }

  fn handle_ping(&mut self, ping: String) {
    let pong = "PO".to_owned() + &ping[2..];
    println!("\x1b[36msending PONG: {}\x1b[0m", pong);
    self.write_line(&pong);
  }

  fn say(&mut self, msg: &str, priv_user: Option<&str>) {
    let header = "PRIVMSG ".to_owned();

    match priv_user {
      Some(user) => {
        self.write_line(&(header + user + " :" + msg));
      },
      None => {
        let channel = &self.channel.clone();
        self.write_line(&(header + channel + " :" + msg));
      }
    }
  }
}

fn is_ping(msg: &str) -> bool {
  msg.starts_with("PING")
}

fn extract_user_msg(msg: &str) -> Option<(Nick, Cmd, Vec<String>)> {
  if !msg.starts_with(":") {
    println!("a");
    return None;
  }

  let msg = &msg[1..]; // remove the first ':'

  // find the index of the first ! to extract nick (left part) and content (right part)
  let bang_index = opt!(msg.find('!'));
  let nick = msg[0 .. bang_index].to_owned();
  let content = &msg[bang_index + 1 ..];

  // [host, cmd, …]
  let items: Vec<_> = content.split(' ').collect();

  if items.len() >= 2 {
    let cmd = items[1].to_owned();

    if items.len() >= 3 {
      let args: Vec<_> = items[2..].iter().map(|&x| x.to_owned()).collect();
      Some((nick, cmd, args))
    } else {
      Some((nick, cmd, Vec::new()))
    }
  } else {
    None
  }
}

fn dispatch_user_msg(irc: &mut IRCClient, re_url: &Regex, re_title: &Regex, nick: Nick, cmd: Cmd, args: Vec<String>) {
  match &cmd[..] {
    "JOIN" => {
      println!("\x1b[36m{} joined!\x1b[0m", nick);
    },
    "PRIVMSG" => {
      treat_privmsg(irc, re_url, re_title, nick, args);
    }
    _ => {}
  }
}

fn treat_privmsg(irc: &mut IRCClient, re_url: &Regex, re_title: &Regex, nick: Nick, args: Vec<String>) {
  // early return to prevent us to talk to ourself
  if nick == irc.nick {
    return;
  }

  let order = extract_order(nick.clone(), &args[1..]);

  match order {
    Some(Order::Tell(from, to, content)) => add_tell(irc, from, to, content),
    None => {
      // someone just said something, see whether we should say something
      if let Some(msgs) = irc.tells.get(&nick).cloned() {
        for &(ref from, ref msg) in msgs.iter() {
          irc.say(&format!("\x02\x036{}\x0F: \x02\x032{}\x0F", from, msg), Some(&nick));
        }

        irc.tells.remove(&nick);
        irc.save_tells();
      }
    }
  }

  // grab the content for further processing
  let content = &args[1..].join(" ")[1..];

  // look for URLs to scan
  let re_match = re_url.find(&content);
  if let Some((start_index, end_index)) = re_match {
    let url = &content[start_index .. end_index];

    let mut headers = header::Headers::new();
    headers.set(
        header::Accept(vec![
          header::qitem(mime::Mime(mime::TopLevel::Text, mime::SubLevel::Html, vec![]))
        ])
    );
    headers.set(
        header::AcceptCharset(vec![
          header::qitem(header::Charset::Ext("utf-8".to_owned()))
        ])
    );

    println!("\x1b[36mGET {}\x1b[0m", url);

    let client = client::Client::new();
    let res = client.get(url).headers(headers).send();

    match res {
      Ok(mut response) => {
        let mut body = String::new();
        let _ = response.read_to_string(&mut body);

        // find the title
        if let Some(captures) = re_title.captures(&body) {
          if let Some(title) = captures.at(1) {
            let channel = if &args[0] == &irc.nick { Some(&nick[..]) } else { None };
            irc.say(&format!("\x037«\x036 {} \x037»\x0F", title), channel);
          }
        }
      },
      Err(e) => {
        println!("\x1b[31munable to get {}: {:?}\x1b[0m", url, e);
      }
    }
  }
}

fn extract_order(from: Nick, msg: &[String]) -> Option<Order> {
  let first = &(&msg[0])[1..]; // remove the ':'

  if first == "!tell" {
    if msg.len() >= 3 {
      // we have someone to tell something
      let to = msg[1].to_owned();
      let content = msg[2..].join(" ");

      return Some(Order::Tell(from, to, content));
    }
  }

  None
}

fn add_tell(irc: &mut IRCClient, from: Nick, to: Nick, content: String) {
  let mut msgs = irc.tells.get(&to).map_or(Vec::new(), |x| x.clone());
  msgs.push((from, content));
  irc.tells.insert(to, msgs);
  irc.save_tells();
}

#[derive(Debug)]
enum Order {
  // from, to, content
  Tell(Nick, Nick, String)
}

type Nick = String;
type Cmd = String;
type Message = String;
type Tells = BTreeMap<Nick, Vec<(Nick, Message)>>;

fn read_tells<P>(path: P) -> Tells where P: AsRef<Path> {
  match File::open(path.as_ref()) {
    Ok(file) => {
      de::from_reader(file).unwrap()
    },
    Err(e) => {
      println!("\x1b[31munable to read tells from {:?}: {}\x1b[0m", path.as_ref(), e);
      Tells::new()
    }
  }
}

fn save_tells<P>(path: P, tells: &Tells) where P: AsRef<Path> {
  match File::create(path.as_ref()) {
    Ok(mut file) => {
      let _ = ser::to_writer(&mut file, tells);
    },
    Err(e) => {
      println!("\x1b[31munable to save tells to {:?}: {}\x1b[0m", path.as_ref(), e);
    }
  }
}

fn main() {
  let host = "irc.freenode.net";
  let port = 6667;
  let mut irc = IRCClient::connect(host, port, "kwak2", "#kwak2");
  let re_url = Regex::new("(^|\\s+)https?://[^ ]+\\.[^ ]+").unwrap();
  let re_title = Regex::new("<title>(.*)</title>").unwrap();

  irc.init();

  loop {
    let line = irc.read_line();
    println!("{}", line);

    if is_ping(&line) {
      irc.handle_ping(line);
      continue;
    }

    if let Some(user_msg) = extract_user_msg(&line) {
      dispatch_user_msg(&mut irc, &re_url, &re_title, user_msg.0, user_msg.1, user_msg.2);
    }
  }
}
