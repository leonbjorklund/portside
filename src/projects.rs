//! Saved projects, one per working directory, in
//! `%APPDATA%\Portside\projects.tsv`: the defaults in the user's order, then
//! running projects and the latest past runs. File order is the only order.

use crate::servers::{Launch, Server};
use std::{
    env, fs,
    io::{ErrorKind, Write},
    path::PathBuf,
};

/// How many stopped projects that aren't defaults are kept.
const PAST_RUNS: usize = 10;

pub struct Project {
    pub default: bool,
    pub port: u16,
    pub launch: Launch,
    /// Its index in the last scan. Not stored.
    pub server: Option<usize>,
}

/// Defaults always come first.
pub struct Projects {
    list: Vec<Project>,
    /// None when the file couldn't be read, so it is never overwritten.
    path: Option<PathBuf>,
}

impl Projects {
    pub fn load() -> Self {
        let path =
            env::var_os("APPDATA").map(|dir| PathBuf::from(dir).join(r"Portside\projects.tsv"));
        match path.as_ref().map(fs::read_to_string) {
            Some(Ok(text)) => Self {
                list: text.lines().filter_map(parse).collect(),
                path,
            },
            Some(Err(error)) if error.kind() == ErrorKind::NotFound => Self {
                list: Vec::new(),
                path,
            },
            _ => Self {
                list: Vec::new(),
                path: None,
            },
        }
    }

    pub fn list(&self) -> &[Project] {
        &self.list
    }

    /// The number of defaults, which is also the first past run's slot.
    pub fn split(&self) -> usize {
        self.list.partition_point(|project| project.default)
    }

    /// Records new projects and matches every project to a server from `servers`.
    pub fn observe(&mut self, servers: &[Server]) {
        for server in servers {
            let Some(launch) = &server.launch else {
                continue;
            };
            if !self.list.iter().any(|p| server.in_folder(&p.launch.dir)) {
                let project = Project {
                    default: false,
                    port: server.port,
                    launch: launch.clone(),
                    server: None,
                };
                self.list.insert(self.split(), project);
            }
        }
        for i in 0..self.list.len() {
            let Project {
                default,
                port,
                launch,
                server,
            } = &mut self.list[i];
            let found = servers
                .iter()
                .position(|s| s.in_folder(&launch.dir) && s.port == *port)
                .or_else(|| {
                    servers.iter().position(|s| match *default {
                        true => s.runs(launch, *port),
                        false => s.in_folder(&launch.dir),
                    })
                });
            if let Some(index) = found {
                *port = servers[index].port;
                // A default keeps its command, so `astro preview` on its port
                // can't replace the command that starts the site.
                if let (false, Some(running)) = (*default, &servers[index].launch) {
                    launch.clone_from(running);
                }
                *server = found;
            } else if server.take().is_some() && !*default {
                let slot = self.split();
                self.list[slot..=i].rotate_right(1);
            }
        }
        let mut stopped = 0;
        self.list.retain(|project| {
            let keep = project.default || project.server.is_some();
            stopped += usize::from(!keep);
            keep || stopped <= PAST_RUNS
        });
        self.save();
    }

    /// Moves a project to the end of the defaults or the front of the past
    /// runs. A new default takes the port and command of the row's `server`.
    pub fn set_default(
        &mut self,
        i: usize,
        default: bool,
        server: Option<usize>,
        servers: &[Server],
    ) {
        let mut project = self.list.remove(i);
        project.default = default;
        if let Some(Server {
            port,
            launch: Some(launch),
            ..
        }) = server.filter(|_| default).map(|index| &servers[index])
        {
            project.port = *port;
            project.launch.clone_from(launch);
        }
        self.list.insert(self.split(), project);
        self.observe(servers);
    }

    /// Forgets a stopped past run until its folder is seen running again.
    pub fn remove_past(&mut self, i: usize) {
        let project = &self.list[i];
        if !project.default && project.server.is_none() {
            self.list.remove(i);
            self.save();
        }
    }

    /// Moves a default during a drag. The caller saves when the drag ends.
    pub fn reorder(&mut self, from: usize, to: usize) {
        let project = self.list.remove(from);
        self.list.insert(to, project);
    }

    /// Writes a temporary file and renames it over the list, so a crash never
    /// leaves a partial file.
    pub fn save(&self) {
        let Some(path) = &self.path else {
            return;
        };
        let temp = path.with_extension(format!("{}.tmp", std::process::id()));
        let text: String = self.list.iter().map(line).collect();
        let result = (|| {
            fs::create_dir_all(path.parent().unwrap_or(path))?;
            let mut file = fs::File::create(&temp)?;
            file.write_all(text.as_bytes())?;
            file.sync_all()?;
            drop(file);
            fs::rename(&temp, path)
        })();
        if result.is_err() {
            let _ = fs::remove_file(temp);
        }
    }
}

/// `default|past`, port, dir, `own|nested`, exe, args, tab-separated.
fn line(project: &Project) -> String {
    let Launch {
        dir,
        exe,
        args,
        nested,
    } = &project.launch;
    let kind = if project.default { "default" } else { "past" };
    let origin = if *nested { "nested" } else { "own" };
    format!("{kind}\t{}\t{dir}\t{origin}\t{exe}\t{args}\n", project.port)
}

fn parse(line: &str) -> Option<Project> {
    let mut fields = line.splitn(6, '\t');
    let default = match fields.next()? {
        "default" => true,
        "past" => false,
        _ => return None,
    };
    let port = fields.next()?.parse().ok()?;
    let dir = fields.next()?.to_owned();
    let nested = match fields.next()? {
        "nested" => true,
        "own" => false,
        _ => return None,
    };
    Some(Project {
        default,
        port,
        launch: Launch {
            dir,
            exe: fields.next()?.to_owned(),
            args: fields.next()?.to_owned(),
            nested,
        },
        server: None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::Ipv4Addr;

    fn launch(dir: &str, args: &str) -> Launch {
        Launch {
            dir: dir.into(),
            exe: r"C:\node.exe".into(),
            args: args.into(),
            nested: false,
        }
    }

    fn server(port: u16, launch: Launch) -> Server {
        Server {
            port,
            name: launch.name().into(),
            address: Ipv4Addr::LOCALHOST.into(),
            launch: Some(launch),
        }
    }

    fn projects<const N: usize>(list: [(bool, u16, Launch); N]) -> Projects {
        Projects {
            list: list
                .into_iter()
                .map(|(default, port, launch)| Project {
                    default,
                    port,
                    launch,
                    server: None,
                })
                .collect(),
            path: None,
        }
    }

    fn ports(projects: &Projects) -> Vec<u16> {
        projects.list().iter().map(|p| p.port).collect()
    }

    #[test]
    fn stored_lines_round_trip_and_malformed_ones_are_skipped() {
        let valid = "default\t3000\tC:\\a\\\town\tC:\\node.exe\tvite.js\t--open\n\
                     past\t8000\tC:\\b\\\tnested\tC:\\python.exe\t\n";
        let stored = format!(
            "other\t1\tC:\\c\\\town\tx\ty\n\
             past\tport\tC:\\c\\\town\tx\ty\n\
             past\t1\tC:\\c\\\tneither\tx\ty\n\
             past\t1\tC:\\c\\\n{valid}"
        );
        let list: Vec<_> = stored.lines().filter_map(parse).collect();
        assert_eq!(list.iter().map(line).collect::<String>(), valid);
    }

    #[test]
    fn a_default_binds_only_its_port_or_its_own_command() {
        let dir = r"C:\site\";
        let saved = launch(dir, "saved");
        let other = launch(dir, "other");
        let sibling = Launch {
            nested: true,
            ..saved.clone()
        };
        for (running, bound, port) in [
            (
                vec![server(3000, saved.clone()), server(3001, saved.clone())],
                Some(1),
                3001,
            ),
            (vec![server(3002, saved.clone())], Some(0), 3002),
            (vec![server(3002, other.clone())], None, 3001),
            (vec![server(3002, sibling.clone())], None, 3001),
            (vec![server(3001, other.clone())], Some(0), 3001),
        ] {
            let mut projects = projects([(true, 3001, saved.clone())]);
            projects.observe(&running);
            let project = &projects.list()[0];
            assert_eq!((project.server, project.port), (bound, port));
            assert_eq!(project.launch.args, "saved");
        }
    }

    #[test]
    fn a_past_run_takes_its_last_port_then_the_lowest() {
        let dir = r"C:\api\";
        for (running, bound, port, args) in [
            ([(8000, "low"), (8001, "last")], 1, 8001, "last"),
            ([(8000, "low"), (8002, "high")], 0, 8000, "low"),
        ] {
            let mut projects = projects([(false, 8001, launch(dir, "old"))]);
            projects.observe(&running.map(|(port, args)| server(port, launch(dir, args))));
            let project = &projects.list()[0];
            assert_eq!((project.server, project.port), (Some(bound), port));
            assert_eq!(project.launch.args, args);
        }
    }

    #[test]
    fn a_newly_stopped_run_moves_to_the_front_of_the_past_runs() {
        let stopped = launch(r"C:\stopped\", "");
        let mut projects = projects([
            (true, 1, launch(r"C:\default\", "")),
            (false, 2, launch(r"C:\older\", "")),
            (false, 3, stopped.clone()),
        ]);
        projects.observe(&[server(3, stopped)]);
        projects.observe(&[]);
        assert_eq!(ports(&projects), [1, 3, 2]);
    }

    #[test]
    fn pruning_keeps_ten_stopped_runs_and_every_running_one() {
        let project = |port: u16| launch(&format!(r"C:\p{port}\"), "");
        let mut projects = projects(std::array::from_fn::<_, 12, _>(|port| {
            (false, port as u16, project(port as u16))
        }));
        projects.observe(&[server(11, project(11))]);
        assert_eq!(ports(&projects), [0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 11]);
    }

    #[test]
    fn only_stopped_past_runs_can_be_forgotten_until_seen_again() {
        let removed = launch(r"C:\removed\", "");
        let mut projects = projects([
            (true, 1, launch(r"C:\stopped-default\", "")),
            (true, 2, launch(r"C:\running-default\", "")),
            (false, 3, launch(r"C:\running\", "")),
            (false, 4, removed.clone()),
            (false, 5, launch(r"C:\other-past\", "")),
        ]);
        let mut running = vec![
            server(2, launch(r"C:\running-default\", "")),
            server(3, launch(r"C:\running\", "")),
        ];
        projects.observe(&running);
        for i in 0..3 {
            projects.remove_past(i);
        }
        assert_eq!(ports(&projects), [1, 2, 3, 4, 5]);

        projects.remove_past(3);
        assert_eq!(ports(&projects), [1, 2, 3, 5]);
        projects.observe(&running);
        assert_eq!(ports(&projects), [1, 2, 3, 5]);

        running.push(server(4, removed));
        projects.observe(&running);
        assert_eq!(ports(&projects), [1, 2, 4, 3, 5]);
    }

    #[test]
    fn plus_and_minus_land_at_the_boundary() {
        let dir = r"C:\d\";
        let mut projects = projects([
            (true, 1, launch(r"C:\a\", "")),
            (true, 2, launch(r"C:\b\", "")),
            (false, 3, launch(r"C:\c\", "")),
            (false, 4, launch(dir, "old")),
        ]);
        let running = [
            server(5, launch(dir, "low")),
            server(6, launch(dir, "clicked")),
        ];
        projects.set_default(3, true, Some(1), &running);
        assert_eq!(ports(&projects), [1, 2, 6, 3]);
        assert_eq!(projects.list()[2].launch.args, "clicked");
        projects.set_default(0, false, None, &running);
        assert_eq!(ports(&projects), [2, 6, 1, 3]);
    }
}
