use crate::{
	ci::{self, fit::Fitness}, discover::{
		comms::{Food, Note}, render::render
	}
};

fn webview_create() -> evscode::R<evscode::Webview> {
	Ok(evscode::Webview::new("icie.discover", "ICIE Discover", 1)
		.enable_scripts()
		.retain_context_when_hidden()
		.create())
}

fn webview_manage(handle: evscode::webview_singleton::Handle) -> evscode::R<()> {
	let (stream, worker_tx) = {
		let view = handle.lock()?;
		view.set_html(render());
		let (worker_tx, worker_rx) = std::sync::mpsc::channel();
		let worker_reports = evscode::LazyFuture::new_worker(move |carrier| worker_thread(carrier, worker_rx));
		let stream = view
			.listener()
			.map(|n| Ok(ManagerMessage::Note(Note::from(n))))
			.join(worker_reports.map(|r| r.map(ManagerMessage::Report)))
			.cancel_on(view.disposer());
		(stream, worker_tx)
	};

	let mut best_fitness = None;
	let mut paused = false;

	for msg in stream {
		let view = handle.lock()?;
		match msg? {
			ManagerMessage::Note(note) => match note {
				Note::Start => {
					paused = false;
					worker_tx.send(WorkerOrder::Start)?;
					view.post_message(Food::State { running: true, reset: false });
				},
				Note::Pause => {
					paused = true;
					worker_tx.send(WorkerOrder::Pause)?;
					view.post_message(Food::State { running: false, reset: false });
				},
				Note::Reset => {
					best_fitness = None;
					paused = false;
					worker_tx.send(WorkerOrder::Reset)?;
					view.post_message(Food::State { running: false, reset: true });
				},
				Note::Save { input } => {
					if !paused {
						paused = true;
						worker_tx.send(WorkerOrder::Pause)?;
					}
					view.post_message(Food::State { running: false, reset: false });
					evscode::spawn(move || add_test_input(input));
				},
			},
			ManagerMessage::Report(report) => match report {
				Ok(row) => {
					let is_failed = !row.solution.verdict.success();
					let new_best = is_failed && best_fitness.map(|bf| bf < row.fitness).unwrap_or(true);
					if new_best {
						best_fitness = Some(row.fitness);
					}
					view.post_message(Food::Row {
						number: row.number,
						outcome: row.solution.verdict,
						fitness: row.fitness,
						input: if new_best { Some(row.input) } else { None },
					});
				},
				Err(e) => {
					best_fitness = None;
					paused = false;
					view.post_message(Food::State { running: false, reset: true });
					evscode::internal::executor::error_show(e);
				},
			},
		}
	}
	Ok(())
}

fn worker_thread(carrier: evscode::future::Carrier<WorkerReport>, orders: std::sync::mpsc::Receiver<WorkerOrder>) -> evscode::R<()> {
	loop {
		match orders.recv() {
			Ok(WorkerOrder::Start) => (),
			Ok(WorkerOrder::Pause) | Ok(WorkerOrder::Reset) => continue,
			Err(std::sync::mpsc::RecvError) => break,
		};
		match worker_run(&carrier, &orders) {
			Ok(()) => (),
			Err(e) => {
				carrier.send(Err(e));
			},
		}
	}
	Ok(())
}

fn worker_run(carrier: &evscode::future::Carrier<WorkerReport>, orders: &std::sync::mpsc::Receiver<WorkerOrder>) -> evscode::R<()> {
	let solution = crate::build::build(crate::dir::solution()?, &ci::cpp::Codegen::Debug)?;
	let brut = crate::build::build(crate::dir::brut()?, &ci::cpp::Codegen::Release)?;
	let gen = crate::build::build(crate::dir::gen()?, &ci::cpp::Codegen::Release)?;
	let task = ci::task::Task {
		checker: Box::new(ci::task::FreeWhitespaceChecker),
		environment: ci::exec::Environment { time_limit: None },
	};
	let mut _status = crate::STATUS.push("Discovering");
	for number in 1.. {
		match orders.try_recv() {
			Ok(WorkerOrder::Start) => (),
			Ok(WorkerOrder::Pause) => {
				drop(_status);
				loop {
					match orders.recv()? {
						WorkerOrder::Start => break,
						WorkerOrder::Pause => (),
						WorkerOrder::Reset => return Err(evscode::E::cancel()),
					}
				}
				_status = crate::STATUS.push("Discovering");
			},
			Ok(WorkerOrder::Reset) => break,
			Err(std::sync::mpsc::TryRecvError::Empty) => (),
			Err(e) => Err(e)?,
		}
		let run_gen = gen.run("", &task.environment)?;
		if !run_gen.success() {
			return Err(evscode::E::error(format!("test generator failed {:?}", run_gen)));
		}
		let input = run_gen.stdout;
		let run_brut = brut.run(&input, &task.environment)?;
		if !run_brut.success() {
			return Err(evscode::E::error(format!("brut failed {:?}", run_brut)));
		}
		let desired = run_brut.stdout;
		let outcome = ci::test::simple_test(&solution, &input, Some(&desired), None, &task)?;
		let fitness = ci::fit::ByteLength.evaluate(&input);
		let row = ci::discover::Row {
			number,
			solution: outcome,
			fitness,
			input,
		};
		carrier.send(Ok(row));
	}
	Ok(())
}

fn add_test_input(input: String) -> evscode::R<()> {
	let _status = crate::STATUS.push("Adding new test");
	let brut = crate::build::build(crate::dir::brut()?, &ci::cpp::Codegen::Release)?;
	let run = brut.run(&input, &ci::exec::Environment { time_limit: None })?;
	if !run.success() {
		return Err(evscode::E::error("brut failed when generating output for the added test"));
	}
	let desired = run.stdout;
	let dir = crate::dir::custom_tests()?;
	std::fs::create_dir_all(&dir)?;
	let used = std::fs::read_dir(&dir)?
		.into_iter()
		.map(|der| {
			der.ok()
				.and_then(|de| de.path().file_stem().map(std::ffi::OsStr::to_owned))
				.and_then(|stem| stem.to_str().map(str::to_owned))
				.and_then(|name| name.parse::<i64>().ok())
		})
		.filter_map(|o| o)
		.collect::<Vec<_>>();
	let id = crate::util::mex(1, used);
	std::fs::write(dir.join(format!("{}.in", id)), input)?;
	std::fs::write(dir.join(format!("{}.out", id)), desired)?;
	crate::test::view()?;
	Ok(())
}

#[derive(Clone)]
enum ManagerMessage {
	Note(Note),
	Report(WorkerReport),
}
enum WorkerOrder {
	Start,
	Pause,
	Reset,
}
type WorkerReport = evscode::R<ci::discover::Row>;

lazy_static::lazy_static! {
	pub static ref WEBVIEW: evscode::WebviewSingleton = evscode::WebviewSingleton::new(webview_create, webview_manage);
}