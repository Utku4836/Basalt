//! Opt-in native smoke run. Does not operate the desktop or touch profile data.
use super::*;
use slint::Model;
use std::{
    cell::Cell,
    fs::{self, File},
    io::BufWriter,
};

pub struct SmokeRun {
    _timer: slint::Timer,
    passed: Rc<Cell<bool>>,
}
pub fn workspace_roundtrip(
    window: &MainWindow,
    controller: Rc<RefCell<Controller>>,
    output: PathBuf,
) -> SmokeRun {
    let timer = slint::Timer::default();
    let weak = window.as_weak();
    let stage = Cell::new(0);
    let ticks = Cell::new(0);
    let passed = Rc::new(Cell::new(false));
    let flag = passed.clone();
    let reopening = !controller.borrow().desktop.workspaces.is_empty();
    window
        .window()
        .set_size(slint::LogicalSize::new(1200.0, 800.0));
    timer.start(slint::TimerMode::Repeated,Duration::from_millis(250),move || {
        let Some(window)=weak.upgrade() else {return;};ticks.set(ticks.get()+1);
        let result=(||->Result<(),Box<dyn std::error::Error>> {
            fs::create_dir_all(&output)?;
            if stage.get()==0 {
                let owner=controller.borrow();
                if !owner.surfaces.is_empty() || !owner.pending.is_empty() {return Err("Startup launched saved terminals before activation".into());}
                if !window.get_empty_workspace() {return Err("Dormant startup is not empty".into());}
                drop(owner);
                save_client_snapshot(&window,&output,if reopening {"dormant-workspaces.png"}else{"empty-application.png"})?;
                if reopening {
                    let id=controller.borrow().desktop.workspaces[0].id;
                    window.invoke_workspace_select(id.to_string().into());stage.set(3);
                } else {
                    window.invoke_chrome_action(2);
                    if !controller.borrow().desktop.current().is_empty() {return Err("New workspace opened a terminal".into());}
                    save_client_snapshot(&window,&output,"new-empty-workspace.png")?;
                    window.invoke_command_execute("workspace-settings".into());
                    save_client_snapshot(&window,&output,"workspace-settings.png")?;
                    // Dismiss the popup with a real click before entering terminal input.
                    use slint::platform::{PointerEventButton,WindowEvent};
                    let position=slint::LogicalPosition::new(20.0,window.get_header_height()+300.0);
                    window.window().dispatch_event(WindowEvent::PointerPressed {position,button:PointerEventButton::Left});
                    window.window().dispatch_event(WindowEvent::PointerReleased {position,button:PointerEventButton::Left});
                    let mut owner=controller.borrow_mut();owner.desktop.current_mut().title="Saved workspace".into();
                    for index in 0..3 {
                        let tab=owner.desktop.execute(Command::NewPaneAutoLayout,CommandTarget::Focused).unwrap();
                        if index==1 {owner.tab_tools.insert(tab,"claude".into());owner.tab_profiles.insert(tab,ProfileId::default_profile());}
                    }
                    let branch=owner.desktop.current().tree.id();owner.desktop.current_mut().tree.divider_mut(branch).unwrap().position=0.37;
                    owner.ensure_terminals();owner.refresh(&window);stage.set(1);
                }
            }
            if stage.get()==1 && controller.borrow().pending.is_empty() {
                let owner=controller.borrow();
                if owner.surfaces.len()!=3 || window.get_panes().row_count()!=3 {return Err("Create did not start exactly three terminals".into());}
                if !owner.failures.is_empty() {return Err(format!("Fixture launch failed: {:?}",owner.failures).into());}
                let managed=owner.tab_tools.keys().copied().next().unwrap();
                let conversation=owner.launches[&managed].conversation.as_ref().unwrap();
                let report=serde_json::json!({"workspace":owner.desktop.selected,"terminal":managed,"conversation":conversation.id,"tree":owner.desktop.current().tree.normalized_bounds().iter().map(|frame|serde_json::json!({"id":frame.id,"x":frame.bounds.x,"y":frame.bounds.y,"w":frame.bounds.width,"h":frame.bounds.height})).collect::<Vec<_>>()});
                fs::write(output.join("saved-identities.json"),serde_json::to_vec_pretty(&report)?)?;
                drop(owner);save_client_snapshot(&window,&output,"saved-layout.png")?;
                flag.set(true);let _=slint::quit_event_loop();stage.set(9);
            }
            if stage.get()==3 && controller.borrow().pending.is_empty() {
                let owner=controller.borrow();
                if owner.surfaces.len()!=3 || window.get_panes().row_count()!=3 {return Err(format!("Restore did not start three terminals: {:?}",owner.failures).into());}
                let prior:serde_json::Value=serde_json::from_slice(&fs::read(output.join("saved-identities.json"))?)?;
                let managed=Uuid::parse_str(prior["terminal"].as_str().unwrap())?;
                let text=owner.surfaces[&managed].terminal.model().snapshot().cells.iter().map(|cell|cell.text.as_str()).collect::<String>();
                if !text.contains("--resume") || !text.contains(prior["conversation"].as_str().unwrap()) {return Ok(());}
                let tree=owner.desktop.current().tree.normalized_bounds().iter().map(|frame|serde_json::json!({"id":frame.id,"x":frame.bounds.x,"y":frame.bounds.y,"w":frame.bounds.width,"h":frame.bounds.height})).collect::<Vec<_>>();
                if serde_json::to_value(tree)?!=prior["tree"] {return Err("Restored split geometry differs".into());}
                if owner.desktop.selected.to_string()!=prior["workspace"].as_str().unwrap() {return Err("Workspace identity changed".into());}
                drop(owner);save_client_snapshot(&window,&output,"restored-layout.png")?;
                let removals={let owner=controller.borrow();owner.desktop.current().tree.pane_ids().into_iter().map(|pane|(pane,owner.desktop.current().tree.pane(pane).unwrap().selected.unwrap())).collect::<Vec<_>>()};
                for (pane,tab) in removals {window.invoke_tab_close(pane.to_string().into(),tab.to_string().into());}
                if !window.get_empty_workspace() || controller.borrow().desktop.workspaces.len()!=1 {return Err("Last terminal close removed the workspace or quit".into());}
                save_client_snapshot(&window,&output,"empty-after-last-terminal.png")?;
                let workspace=controller.borrow().desktop.selected;window.invoke_workspace_close(workspace.to_string().into());
                if !controller.borrow().desktop.workspaces.is_empty() || !window.get_empty_workspace() {return Err("Last workspace deletion did not leave an empty app".into());}
                save_client_snapshot(&window,&output,"empty-after-last-workspace.png")?;
                let report=serde_json::json!({"passed":true,"startup_dormant":true,"empty_new_workspace":true,"three_terminals_restored":true,"same_cli_conversation_argument":true,"same_workspace_tree":true,"last_terminal_and_workspace_keep_app_open":true,"scope":"Real owned CMD + profile-owned argv fixture; no model/vendor calls"});
                fs::write(output.join("workspace-roundtrip.json"),serde_json::to_vec_pretty(&report)?)?;flag.set(true);let _=slint::quit_event_loop();stage.set(9);
            }
            Ok(())
        })();
        if let Err(error)=result {let _=fs::write(output.join("workspace-error.txt"),error.to_string());let _=slint::quit_event_loop();}
        if ticks.get()>100 {let _=fs::write(output.join("workspace-error.txt"),format!("Timed out at {stage:?}"));let _=slint::quit_event_loop();}
    });
    SmokeRun {
        _timer: timer,
        passed,
    }
}
pub fn responsiveness(
    window: &MainWindow,
    controller: Rc<RefCell<Controller>>,
    output: PathBuf,
) -> SmokeRun {
    let timer = slint::Timer::default();
    let weak = window.as_weak();
    let ticks = Cell::new(0);
    let stage = Cell::new(0);
    let passed = Rc::new(Cell::new(false));
    let flag = passed.clone();
    let callbacks = RefCell::new(Vec::<f64>::new());
    let ticks_at = Cell::new(None::<std::time::Instant>);
    let tick_intervals = RefCell::new(Vec::<f64>::new());
    let frames = Rc::new(RefCell::new(Vec::<f64>::new()));
    let starts = Rc::new(RefCell::new(Vec::<std::time::Instant>::new()));
    let rendering = Rc::new(Cell::new(None::<std::time::Instant>));
    let graphics = Rc::new(RefCell::new(String::new()));
    let durations = frames.clone();
    let frame_starts = starts.clone();
    let render_start = rendering.clone();
    let api_name = graphics.clone();
    let notifier = window
        .window()
        .set_rendering_notifier(move |state, api| match state {
            slint::RenderingState::BeforeRendering => {
                let now = std::time::Instant::now();
                render_start.set(Some(now));
                let _ = &frame_starts;
                *api_name.borrow_mut() = format!("{api:?}");
            }
            slint::RenderingState::AfterRendering => {
                if let Some(start) = render_start.take() {
                    durations
                        .borrow_mut()
                        .push(start.elapsed().as_secs_f64() * 1000.0);
                }
            }
            _ => {}
        })
        .is_ok();
    window
        .window()
        .set_size(slint::LogicalSize::new(1200.0, 900.0));
    let redraws = starts.clone();
    window.window().on_winit_window_event(move |_, event| {
        if matches!(
            event,
            slint::winit_030::winit::event::WindowEvent::RedrawRequested
        ) {
            redraws.borrow_mut().push(std::time::Instant::now());
        }
        slint::winit_030::EventResult::Propagate
    });
    window
        .window()
        .with_winit_window(|native| native.set_visible(true));

    timer.start(slint::TimerMode::Repeated,Duration::from_millis(8),move || {
        let Some(window)=weak.upgrade() else {return;};ticks.set(ticks.get()+1);
        if stage.get()==0 && ticks.get()>30 && controller.borrow().pending.is_empty() {
            for _ in 0..3 {window.invoke_chrome_action(6);}stage.set(1);return;
        }
        if stage.get()==1 && controller.borrow().pending.is_empty() {
            let owner=controller.borrow();for surface in owner.surfaces.values() {let _=surface.terminal.input(b"for /L %i in (1,1,45) do @echo 0123456789012345678901234567890123456789012345678901234567890123456789\r".to_vec());}
            drop(owner);
            {let mut owner=controller.borrow_mut();let ids=owner.surfaces.keys().copied().collect::<Vec<_>>();for id in ids {owner.tab_tools.insert(id,"codex".into());if let Some(bridge)=&owner.bridge {let signal=basalt::agent_bridge::Signal {provider:"codex".into(),state:"idle".into(),session_id:"performance-fixture".into(),turn_id:String::new(),event:"SessionStart".into()};let _=fs::write(bridge.path(id),serde_json::to_vec(&signal).unwrap());}}}
            stage.set(2);ticks.set(0);return;
        }
        if stage.get()==2 && ticks.get()>120 {frames.borrow_mut().clear();starts.borrow_mut().clear();ticks.set(0);stage.set(3);}
        if stage.get()==3 {
            let now=std::time::Instant::now();if let Some(previous)=ticks_at.replace(Some(now)) {tick_intervals.borrow_mut().push((now-previous).as_secs_f64()*1000.0);}
            let start=std::time::Instant::now();window.set_sidebar_width(208.0+(ticks.get()%2) as f32);controller.borrow_mut().refresh(&window);callbacks.borrow_mut().push(start.elapsed().as_secs_f64()*1000.0);window.window().request_redraw();
            if callbacks.borrow().len()>=120 {
                let percentile=|values:&[f64],q:f64| {let mut values=values.to_vec();values.sort_by(f64::total_cmp);values.get(((values.len().saturating_sub(1)) as f64*q) as usize).copied().unwrap_or(0.0)};
                let intervals=starts.borrow().windows(2).map(|w|(w[1]-w[0]).as_secs_f64()*1000.0).collect::<Vec<_>>();
                let rows=callbacks.borrow();let p95=percentile(&rows,0.95);let report=serde_json::json!({"passed":p95<12.0 && percentile(&intervals,0.95)<30.0 && intervals.len()>30,"ui_refresh_p50_ms":percentile(&rows,0.5),"ui_refresh_p95_ms":p95,"render_p95_ms":if frames.borrow().is_empty() {None} else {Some(percentile(&frames.borrow(),0.95))},"frame_interval_p50_ms":percentile(&intervals,0.5),"frame_interval_p95_ms":percentile(&intervals,0.95),"render_frames":starts.borrow().len(),"timer_interval_p95_ms":percentile(&tick_intervals.borrow(),0.95),"notifier_available":notifier,"graphics_api":if graphics.borrow().is_empty() {None} else {Some(graphics.borrow().clone())},"requested_renderer":if std::env::args().any(|a|a=="--software-renderer-baseline") {"skia-software-baseline"} else {"skia-d3d"},"scope":"Four populated owned CMD panes and controlled lifecycle signals, repeated sidebar geometry changes, no GPU readback; render intervals are not display presentation FPS"});
                let written=fs::create_dir_all(&output).and_then(|_|fs::write(output.join("responsiveness.json"),serde_json::to_vec_pretty(&report).unwrap())).is_ok();flag.set(written);let _=slint::quit_event_loop();
            }
        }
    });
    SmokeRun {
        _timer: timer,
        passed,
    }
}
pub fn live_codex_hook(
    window: &MainWindow,
    controller: Rc<RefCell<Controller>>,
    output: PathBuf,
) -> SmokeRun {
    let timer = slint::Timer::default();
    let weak = window.as_weak();
    let started = Cell::new(false);
    let ticks = Cell::new(0);
    let passed = Rc::new(Cell::new(false));
    let flag = passed.clone();
    timer.start(slint::TimerMode::Repeated,Duration::from_millis(250),move || {
        let Some(window)=weak.upgrade() else {return;};ticks.set(ticks.get()+1);
        if !started.get() && ticks.get()>4 && controller.borrow().pending.is_empty() {
            window.invoke_key_input("codex".into(),false,false,false,false);window.invoke_key_input(char::from(slint::platform::Key::Return).to_string().into(),false,false,false,false);started.set(true);
        }
        let signal={let owner=controller.borrow();owner.desktop.current().selected_tab().and_then(|id|owner.bridge.as_ref().and_then(|bridge|bridge.read(id)))};
        let success=signal.as_ref().is_some_and(|s|s.provider=="codex" && s.state=="idle");
        if success || ticks.get()>60 {
            let text={let owner=controller.borrow();owner.surfaces.values().flat_map(|s|s.terminal.model().snapshot().cells).map(|c|c.text).collect::<String>().to_lowercase()};
            let report=serde_json::json!({"live_session_start_verified":success,"codex_process_detected":controller.borrow().activities.values().any(|a|a.name=="Codex"),"command_not_found":text.contains("not recognized")||text.contains("tanınmıyor"),"error_visible":text.contains("error:")||text.contains("failed"),"hook_review_visible":text.contains("hook") && (text.contains("trust")||text.contains("review")),"authentication_visible":text.contains("sign in")||text.contains("log in"),"folder_trust_visible":text.contains("trust this")||text.contains("trust the"),"scope":"Installed Codex launched without sending any prompt; transcript/text not saved"});
            let written=fs::create_dir_all(&output).and_then(|_|fs::write(output.join("live-codex-hook.json"),serde_json::to_vec_pretty(&report).unwrap())).is_ok();flag.set(written);let _=slint::quit_event_loop();
        }
    });
    SmokeRun {
        _timer: timer,
        passed,
    }
}
pub fn agent_hooks(
    window: &MainWindow,
    controller: Rc<RefCell<Controller>>,
    output: PathBuf,
) -> SmokeRun {
    let timer = slint::Timer::default();
    let weak = window.as_weak();
    let stage = Cell::new(0);
    let ticks = Cell::new(0);
    let working_at = Cell::new(0);
    let passed = Rc::new(Cell::new(false));
    let flag = passed.clone();
    fs::create_dir_all(&output).unwrap();
    let fixture = output.join("codex.js");
    let script = std::env::current_exe().unwrap();
    fs::write(&fixture, include_str!("fixtures/agent_cli.js")).unwrap();
    let node = std::env::var_os("BASALT_TEST_NODE")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("C:/Program Files/nodejs/node.exe"));
    window
        .window()
        .set_size(slint::LogicalSize::new(1000.0, 700.0));
    timer.start(slint::TimerMode::Repeated,Duration::from_millis(250),move || {
        let Some(window)=weak.upgrade() else {return;};ticks.set(ticks.get()+1);
        let send=|line:String| {window.invoke_key_input(line.into(),false,false,false,false);window.invoke_key_input(char::from(slint::platform::Key::Return).to_string().into(),false,false,false,false);};
        if stage.get()==0 && ticks.get()>=5 && controller.borrow().pending.is_empty() {
            if window.get_agents().row_count()!=0 {let _=slint::quit_event_loop();return;}
            send(format!("\"{}\" \"{}\" \"{}\"",node.display(),fixture.display(),script.display()));stage.set(1);
        }
        if stage.get()==1 && window.get_agents().iter().any(|a|a.name=="Codex" && a.state==0) {let _=save_client_snapshot(&window,&output,"agent-idle.png");send("working".into());stage.set(2);}
        if stage.get()==2 && window.get_agents().iter().any(|a|a.state==1) {working_at.set(ticks.get());stage.set(3);}
        if stage.get()==3 && ticks.get()>=working_at.get()+12 {
            if !window.get_agents().iter().any(|a|a.state==1) {let _=slint::quit_event_loop();return;}
            let _=save_client_snapshot(&window,&output,"agent-working-quiet.png");send("stop".into());stage.set(4);
        }
        if stage.get()==4 && window.get_agents().iter().any(|a|a.state==0) {send("exit".into());stage.set(5);}
        if stage.get()==5 && window.get_agents().row_count()==0 {
            let pane=controller.borrow().desktop.current().focused;window.invoke_pane_action(pane.to_string().into(),2);stage.set(6);
        }
        if stage.get()==6 && window.get_dividers().row_count()==1 {
            use slint::platform::{PointerEventButton,WindowEvent};
            let divider=window.get_dividers().row_data(0).unwrap();let left=window.get_sidebar_width()+1.0;let y=window.get_header_height()+80.0;
            let position=slint::LogicalPosition::new(left+divider.left,y);
            window.window().dispatch_event(WindowEvent::PointerMoved {position});window.window().dispatch_event(WindowEvent::PointerPressed {position,button:PointerEventButton::Left});
            for delta in [20.0,60.0,140.0,230.0] {
                window.window().dispatch_event(WindowEvent::PointerMoved {position:slint::LogicalPosition::new(left+divider.left+delta,y)});let _=window.window().take_snapshot();
            }
            window.window().dispatch_event(WindowEvent::PointerReleased {position:slint::LogicalPosition::new(left+divider.left+230.0,y),button:PointerEventButton::Left});
            let moved=window.get_dividers().row_data(0).unwrap().left>=divider.left+229.0;
            let _=save_client_snapshot(&window,&output,"continuous-divider-drag.png");
            let report=serde_json::json!({"passed":moved,"shells_hidden":true,"hook_idle_working_stop_exit":true,"quiet_working_not_timed_out":true,"continuous_native_divider_drag":moved,"scope":"Actual Node fixture, the real hook program and owned PTY; no vendor/model prompt calls"});
            let written=fs::write(output.join("agent-hooks-divider.json"),serde_json::to_vec_pretty(&report).unwrap()).is_ok();flag.set(moved&&written);let _=slint::quit_event_loop();
        }
        if ticks.get()>120 {let _=slint::quit_event_loop();}
    });
    SmokeRun {
        _timer: timer,
        passed,
    }
}
pub fn typing_latency(
    window: &MainWindow,
    controller: Rc<RefCell<Controller>>,
    output: PathBuf,
) -> SmokeRun {
    let timer = slint::Timer::default();
    let weak = window.as_weak();
    let passed = Rc::new(Cell::new(false));
    let flag = passed.clone();
    let stage = Cell::new(0);
    let stable = Cell::new(0);
    let last = Cell::new(0);
    let pending = RefCell::new(None::<(std::time::Instant, u64, f64)>);
    let samples = RefCell::new(Vec::<serde_json::Value>::new());
    window
        .window()
        .set_size(slint::LogicalSize::new(1000.0, 700.0));
    timer.start(slint::TimerMode::Repeated,Duration::from_millis(20),move || {
        let Some(window)=weak.upgrade() else {return;};
        let (revision,run_count,render_items)= {
            let owner=controller.borrow();let Some(id)=owner.desktop.current().selected_tab() else {return;};
            let Some(surface)=owner.surfaces.get(&id) else {return;};
            if stage.get()==0 {
                let _=surface.terminal.input(b"for /L %i in (1,1,45) do @echo 0123456789012345678901234567890123456789012345678901234567890123456789\r".to_vec());
                stage.set(1);return;
            }
            {let runs=screen::flat_runs(&surface.frame);(surface.revision,runs.iter().map(|run|run.cells as usize).sum::<usize>(),runs.row_count())}
        };
        if stage.get()==1 {
            if run_count<1000 {return;}
            if last.replace(revision)==revision {stable.set(stable.get()+1);} else {stable.set(0);}
            if stable.get()<8 {return;}
            let _=window.window().take_snapshot();stage.set(2);
        }
        if let Some((start,before,callback_ms))=*pending.borrow() {
            if revision==before {return;}
            let projection_ms=start.elapsed().as_secs_f64()*1000.0;
            let capture=std::time::Instant::now();let snapshot=window.window().take_snapshot();
            let capture_ms=capture.elapsed().as_secs_f64()*1000.0;
            samples.borrow_mut().push(serde_json::json!({"key_callback_ms":callback_ms,"key_to_model_ms":projection_ms,"snapshot_ms":capture_ms,"key_to_snapshot_ms":start.elapsed().as_secs_f64()*1000.0,"snapshot_ok":snapshot.is_ok(),"glyph_count":run_count,"render_items":render_items}));
        }
        *pending.borrow_mut()=None;
        if samples.borrow().len()==8 {
            let rows=samples.borrow();let max=rows.iter().filter_map(|row|row["key_to_snapshot_ms"].as_f64()).fold(0.0_f64,f64::max);
            let success=max<80.0 && rows.iter().all(|row|row["snapshot_ok"]==true);
            let report=serde_json::json!({"passed":success,"max_key_to_snapshot_ms":max,"samples":*rows,"build":"debug_assertions=".to_string()+&cfg!(debug_assertions).to_string(),"scope":"Actual key dispatch to owned CMD PTY, production projection, renderer snapshot/readback on populated screen; not hardware presentation FPS"});
            let written=fs::create_dir_all(&output).and_then(|_|fs::write(output.join("typing-latency.json"),serde_json::to_vec_pretty(&report).unwrap())).is_ok();
            flag.set(success&&written);let _=slint::quit_event_loop();return;
        }
        use slint::platform::WindowEvent;
        let start=std::time::Instant::now();
        window.window().dispatch_event(WindowEvent::KeyPressed {text:"x".into()});
        window.window().dispatch_event(WindowEvent::KeyReleased {text:"x".into()});
        *pending.borrow_mut()=Some((start,revision,start.elapsed().as_secs_f64()*1000.0));
    });
    SmokeRun {
        _timer: timer,
        passed,
    }
}
pub fn last_workspace_close(
    window: &MainWindow,
    controller: Rc<RefCell<Controller>>,
    output: PathBuf,
) -> SmokeRun {
    let timer = slint::Timer::default();
    let weak = window.as_weak();
    let passed = Rc::new(Cell::new(false));
    let flag = passed.clone();
    timer.start(slint::TimerMode::SingleShot,Duration::from_millis(750),move || {
        let Some(window)=weak.upgrade() else {return;};
        let original=controller.borrow().desktop.selected;
        use slint::platform::{PointerEventButton,WindowEvent};
        let mut context_creates_visible_panes=true;
        for expected in 2..=5 {
            let pane=window.get_panes().iter().find(|pane|pane.focused).unwrap();
            let left=if window.get_sidebar_visible() {window.get_sidebar_width()+1.0} else {0.0};
            let click=|position,button| {
                window.window().dispatch_event(WindowEvent::PointerMoved {position});
                window.window().dispatch_event(WindowEvent::PointerPressed {position,button});
                window.window().dispatch_event(WindowEvent::PointerReleased {position,button});
            };
            click(slint::LogicalPosition::new(left+pane.left+60.0,window.get_header_height()+pane.top+70.0),PointerEventButton::Right);
            let menu_x=60.0_f32.min((pane.width-190.0).max(0.0));
            let menu_y=70.0_f32.min((pane.height-98.0).max(0.0));
            click(slint::LogicalPosition::new(left+pane.left+menu_x+30.0,window.get_header_height()+pane.top+menu_y+20.0),PointerEventButton::Left);
            context_creates_visible_panes &= controller.borrow().desktop.current().tree.pane_ids().len()==expected;
        }
        let _=save_client_snapshot(&window,&output,"balanced-five-panels.png");
        let pane=window.get_panes().iter().find(|pane|pane.focused).unwrap();
        let closing=Uuid::parse_str(&pane.selected_tab).unwrap();
        let left=window.get_sidebar_width()+1.0;
        let click=|position,button| {
            window.window().dispatch_event(WindowEvent::PointerMoved {position});
            window.window().dispatch_event(WindowEvent::PointerPressed {position,button});
            window.window().dispatch_event(WindowEvent::PointerReleased {position,button});
        };
        click(slint::LogicalPosition::new(left+pane.left+60.0,window.get_header_height()+pane.top+70.0),PointerEventButton::Right);
        let menu_x=60.0_f32.min((pane.width-190.0).max(0.0));
        let menu_y=70.0_f32.min((pane.height-98.0).max(0.0));
        click(slint::LogicalPosition::new(left+pane.left+menu_x+30.0,window.get_header_height()+pane.top+menu_y+80.0),PointerEventButton::Left);
        let context_close_retires_selected=!controller.borrow().desktop.contains_tab(closing) && controller.borrow().desktop.current().tree.pane_ids().len()==4;
        window.invoke_chrome_action(2);
        let survivor=controller.borrow().desktop.selected;
        window.invoke_workspace_close(original.to_string().into());
        let nonfinal_kept_window=window.window().is_visible() && controller.borrow().desktop.workspaces.len()==1 && controller.borrow().desktop.selected==survivor;
        window.invoke_workspace_close(survivor.to_string().into());
        let no_replacement=controller.borrow().desktop.selected==survivor;
        let closed=!window.window().is_visible();
        let success=context_close_retires_selected&&context_creates_visible_panes&&nonfinal_kept_window&&no_replacement&&closed;
        let report=serde_json::json!({"passed":success,"right_click_close_retires_selected_terminal":context_close_retires_selected,"right_click_new_terminal_creates_visible_panes":context_creates_visible_panes,"nonfinal_close_preserves_window":nonfinal_kept_window,"last_close_hides_window":closed,"no_replacement_workspace":no_replacement});
        let written=fs::create_dir_all(&output).and_then(|_|fs::write(output.join("workspace-close.json"),serde_json::to_vec_pretty(&report).unwrap())).is_ok();
        flag.set(success&&written);let _=slint::quit_event_loop();
    });
    SmokeRun {
        _timer: timer,
        passed,
    }
}
/// Measures the real right-click callback and renderer readback separately.
/// Opt-in, own window only; no user configuration or external UI interaction.
pub fn menu_latency(
    window: &MainWindow,
    controller: Rc<RefCell<Controller>>,
    output: PathBuf,
) -> SmokeRun {
    let timer = slint::Timer::default();
    let weak = window.as_weak();
    let ticks = Cell::new(0);
    let samples = RefCell::new(Vec::<serde_json::Value>::new());
    let passed = Rc::new(Cell::new(false));
    let flag = passed.clone();
    window
        .window()
        .set_size(slint::LogicalSize::new(1000.0, 700.0));
    timer.start(slint::TimerMode::Repeated,Duration::from_millis(250),move || {
        let Some(window)=weak.upgrade() else {return;};ticks.set(ticks.get()+1);
        if ticks.get()<8 || !controller.borrow().pending.is_empty() {return;}
        use slint::platform::{PointerEventButton,WindowEvent};
        let point=slint::LogicalPosition::new(window.get_sidebar_width()+70.0,window.get_header_height()+100.0);
        let left=slint::LogicalPosition::new(window.get_sidebar_width()+80.0,window.get_header_height()+330.0);
        window.window().dispatch_event(WindowEvent::PointerMoved {position:left});
        window.window().dispatch_event(WindowEvent::PointerPressed {position:left,button:PointerEventButton::Left});
        window.window().dispatch_event(WindowEvent::PointerReleased {position:left,button:PointerEventButton::Left});
        let baseline=std::time::Instant::now();let base=window.window().take_snapshot();let base_ms=baseline.elapsed().as_secs_f64()*1000.0;
        let start=std::time::Instant::now();
        window.window().dispatch_event(WindowEvent::PointerMoved {position:point});
        window.window().dispatch_event(WindowEvent::PointerPressed {position:point,button:PointerEventButton::Right});
        window.window().dispatch_event(WindowEvent::PointerReleased {position:point,button:PointerEventButton::Right});
        let callback_ms=start.elapsed().as_secs_f64()*1000.0;
        let render=std::time::Instant::now();let snapshot=window.window().take_snapshot();let render_ms=render.elapsed().as_secs_f64()*1000.0;
        samples.borrow_mut().push(serde_json::json!({"callback_ms":callback_ms,"snapshot_ms":render_ms,"baseline_snapshot_ms":base_ms,"snapshot_ok":base.is_ok()&&snapshot.is_ok()}));
        if samples.borrow().len()<8 {return;}
        let rows=samples.borrow();let max_callback=rows.iter().filter_map(|row|row["callback_ms"].as_f64()).fold(0.0_f64,f64::max);
        let max_extra=rows.iter().filter_map(|row|Some(row["snapshot_ms"].as_f64()?-row["baseline_snapshot_ms"].as_f64()?)).fold(0.0_f64,f64::max);
        let success=max_callback<50.0 && max_extra<100.0 && rows.iter().all(|row|row["snapshot_ok"]==true);
        let report=serde_json::json!({"passed":success,"max_callback_ms":max_callback,"max_extra_snapshot_ms":max_extra,"samples":*rows,"scope":"actual production right-click, own-window renderer snapshot; snapshot is not hardware presentation latency"});
        let result=fs::create_dir_all(&output).and_then(|_|fs::write(output.join("menu-latency.json"),serde_json::to_vec_pretty(&report).unwrap()));flag.set(success&&result.is_ok());let _=slint::quit_event_loop();
    });
    SmokeRun {
        _timer: timer,
        passed,
    }
}
impl SmokeRun {
    pub fn passed(&self) -> bool {
        self.passed.get()
    }
}
pub fn start(
    window: &MainWindow,
    controller: Rc<RefCell<Controller>>,
    output: PathBuf,
) -> SmokeRun {
    let timer = slint::Timer::default();
    let weak = window.as_weak();
    let ticks = Cell::new(0);
    let stage = Cell::new(0);
    let original = controller.borrow().desktop.selected;
    let verify_dock = std::env::args().any(|arg| arg == "--verify-dock");
    let dock_input_sent = Cell::new(false);
    let verify_canvas = std::env::args().any(|arg| arg == "--verify-canvas");
    let canvas_input_sent = Cell::new(false);
    let split_before_canvas = RefCell::new(None);
    let verify_resize = std::env::args().any(|arg| arg == "--verify-resize");
    let verify_dpi = std::env::args().any(|arg| arg == "--verify-dpi");
    let verify_typography = std::env::args().any(|arg| arg == "--verify-typography");
    let verify_fonts = std::env::args().any(|arg| arg == "--verify-font-magnification");
    let verify_glyphs = std::env::args().any(|arg| arg == "--verify-glyphs");
    let verify_titles = std::env::args().any(|arg| arg == "--verify-titles");
    let verify_interaction = std::env::args().any(|arg| arg == "--verify-interaction");
    let verify_app_chrome = std::env::args().any(|arg| arg == "--verify-app-chrome");
    let chrome_capture_errors = RefCell::new(Vec::<String>::new());
    let interaction_tab = Cell::new(None);
    let startup_input_seen = Cell::new(false);
    let initial_tab = controller
        .borrow()
        .desktop
        .current()
        .selected_tab()
        .unwrap();
    let title_reset_seen = Cell::new(false);
    let background_title_seen = Cell::new(false);
    if verify_titles {
        window.invoke_workspace_rename(original.to_string().into(), "My workspace".into());
    }
    let grid_before_font = RefCell::new(None);
    if verify_fonts {
        window.invoke_font_change(100);
    }
    let dpi_event_sent = Cell::new(false);
    let grid_before_dpi = RefCell::new(None);
    let grid_before_resize = RefCell::new(None);
    let original_marker_seen = Cell::new(false);
    let canvas_marker_seen = Cell::new(false);
    // Test commands use component callbacks. Keep physical mouse/keyboard input
    // from changing the fixture while a user is working on the same desktop.
    // Input isolation lives in the shared production filter, alongside DPI.
    window.window().with_winit_window(|native| {
        let _ =
            native.request_inner_size(slint::winit_030::winit::dpi::PhysicalSize::new(2560, 1440));
    });
    let trace = RefCell::new(Vec::<serde_json::Value>::new());
    let passed = Rc::new(Cell::new(false));
    let result_flag = passed.clone();
    timer.start(slint::TimerMode::Repeated, Duration::from_millis(250), move || {
        ticks.set(ticks.get() + 1);
        let Some(window) = weak.upgrade() else { return; };
        if ticks.get()==2 {
            let scale=window.window().scale_factor();
            window.window().set_size(slint::LogicalSize::new(2560.0/scale,1440.0/scale));
        }
        if stage.get() == 0 {
            let ready = {
                let owner = controller.borrow();
                owner.desktop.current().selected_tab().and_then(|id| owner.surfaces.get(&id)).is_some_and(|surface| {
                    surface.terminal.input(b"echo BASALT_PORT_PTY_OK\r".to_vec()).is_ok()
                })
            };
            if ready {
                let pane = controller.borrow().desktop.current().focused.to_string();
                window.invoke_pane_action(pane.into(), 2);
                let pane = controller.borrow().desktop.current().focused.to_string();
                window.invoke_pane_action(pane.into(), 2);
                window.invoke_chrome_action(2); window.invoke_chrome_action(4);
                trace.borrow_mut().push(fixture(&controller.borrow(),"created"));
                stage.set(1);
            }
        }
        if verify_interaction && ticks.get() < 24 {
            startup_input_seen.set(startup_input_seen.get() || controller.borrow().surfaces.get(&initial_tab).is_some_and(|surface|surface.terminal.title() == "BASALT_STARTUP_KEY_OK"));
        }
        if stage.get() == 1 && ticks.get() >= 24 {
            let (source,tab,x,y) = {
                let owner=controller.borrow();let workspace=owner.desktop.current();
                let layout=workspace.tree.layout(owner.viewport,1.0).unwrap();
                let first=&layout.panes[0]; let second=&layout.panes[1];
                (first.id,workspace.tree.pane(first.id).unwrap().tabs[0].id,second.bounds.x+second.bounds.width/2.0,second.bounds.y+second.bounds.height/2.0)
            };
            // Move the existing PTY to another pane and return it to a tab strip.
            window.invoke_tab_drag(source.to_string().into(),tab.to_string().into(),x as f32,y as f32,true);
            trace.borrow_mut().push(fixture(&controller.borrow(),"moved"));
            let moved_pane=controller.borrow().desktop.current().focused;
            window.invoke_tab_drag(moved_pane.to_string().into(),tab.to_string().into(),10.0,10.0,true);
            trace.borrow_mut().push(fixture(&controller.borrow(),"returned"));
            stage.set(2);
        }
        if stage.get() == 2 && ticks.get() == 30 { let missing=12-controller.borrow().desktop.current().tree.pane_ids().len(); for _ in 0..missing { let pane=controller.borrow().desktop.current().focused.to_string(); window.invoke_pane_action(pane.into(),2); } trace.borrow_mut().push(fixture(&controller.borrow(),"twelve-pane-stress")); }
        if verify_fonts && ticks.get()==32 {
            *grid_before_font.borrow_mut()=controller.borrow().desktop.focused_container().selected_tab().and_then(|id|controller.borrow().surfaces.get(&id).map(|surface|surface.size));
            window.invoke_font_change(200);
        }
        if verify_dock && ticks.get() == 32 {
            window.invoke_command_execute("toggle-dock".into());
            for _ in 0..2 {
                let pane=controller.borrow().desktop.focused_container().focused.to_string();
                window.invoke_pane_action(pane.into(),2);
            }
            trace.borrow_mut().push(fixture(&controller.borrow(),"dock-created-main-preserved"));
        }
        if verify_dock && ticks.get() >= 36 && !dock_input_sent.get() {
            let ready={let owner=controller.borrow();owner.desktop.focused_container().selected_tab().is_some_and(|id|owner.surfaces.contains_key(&id))};
            if ready {
                window.invoke_key_input("echo BASALT_DOCK_PTY_OK".into(),false,false,false,false);
                window.invoke_key_input(char::from(slint::platform::Key::Return).to_string().into(),false,false,false,false);
                dock_input_sent.set(true);
            }
        }
        if verify_canvas && ticks.get() == 32 {
            *split_before_canvas.borrow_mut()=Some(controller.borrow().desktop.current().tree.normalized_bounds());
            window.invoke_command_execute("toggle-canvas-layout".into());
            let pane=controller.borrow().desktop.current().focused_view_pane().to_string();
            window.invoke_pane_action(pane.into(),2);
        }
        if verify_canvas && ticks.get() >= 36 && !canvas_input_sent.get() {
            let ready={let owner=controller.borrow();owner.desktop.current().selected_tab().is_some_and(|id|owner.surfaces.contains_key(&id))};
            if ready {
                window.invoke_key_input("echo BASALT_CANVAS_PTY_OK".into(),false,false,false,false);
                window.invoke_key_input(char::from(slint::platform::Key::Return).to_string().into(),false,false,false,false);
                canvas_input_sent.set(true);
            }
        }
        if verify_canvas && ticks.get()==40 {window.invoke_canvas_pan(100.0,40.0);}
        if ticks.get()>=8 {
            let owner=controller.borrow();
            original_marker_seen.set(original_marker_seen.get() || owner.surfaces.values().any(|surface|marker_present(surface,"BASALT_PORT_PTY_OK")));
            canvas_marker_seen.set(canvas_marker_seen.get() || owner.desktop.current().canvas.panes.iter().any(|pane|owner.surfaces.get(&pane.selected).is_some_and(|surface|marker_present(surface,"BASALT_CANVAS_PTY_OK"))));
        }
        if verify_resize && ticks.get()==42 {
            *grid_before_resize.borrow_mut()=controller.borrow().desktop.focused_container().selected_tab().and_then(|id|controller.borrow().surfaces.get(&id).map(|surface|surface.size));
            window.window().set_size(slint::LogicalSize::new(800.0,500.0));
        }
        if verify_dpi && ticks.get()==43 {
            *grid_before_dpi.borrow_mut()=controller.borrow().desktop.focused_container().selected_tab().and_then(|id|controller.borrow().surfaces.get(&id).map(|surface|surface.size));
            #[cfg(all(windows,debug_assertions))]
            {dpi_event_sent.set(window.window().with_winit_window(|native|window_frame::simulate_dpi_for_test(native,144)).unwrap_or(false));}
        }
        if verify_interaction && ticks.get() == 46 {
            use slint::platform::{PointerEventButton, WindowEvent};
            let pane = window.get_panes().row_data(0).unwrap();
            interaction_tab.set(Some(Uuid::parse_str(&pane.tabs.iter().find(|tab|tab.selected).unwrap().id).unwrap()));
            let position = slint::LogicalPosition::new(
                (if window.get_sidebar_visible() { window.get_sidebar_width() + 1.0 } else { 0.0 }) + pane.left + 30.0,
                window.get_header_height() + pane.top + controller.borrow().tabbar_height as f32 + 25.0,
            );
            window.window().dispatch_event(WindowEvent::PointerMoved { position });
            window.window().dispatch_event(WindowEvent::PointerPressed { position, button: PointerEventButton::Left });
            window.window().dispatch_event(WindowEvent::PointerReleased { position, button: PointerEventButton::Left });
            // An executed title command is durable even when a tiny resized
            // terminal scrolls both the command and its echo out of the grid.
            for text in "title BASALT_POINTER_KEY_OK".chars().chain(std::iter::once(char::from(slint::platform::Key::Return))) {
                let text = text.to_string();
                window.window().dispatch_event(WindowEvent::KeyPressed { text: text.clone().into() });
                window.window().dispatch_event(WindowEvent::KeyReleased { text: text.into() });
            }
        }
        if verify_titles && ticks.get()==32 {
            let owner=controller.borrow();
            if let Some(surface)=owner.surfaces.get(&initial_tab) {surface.terminal.input(b"title:Build Console\r".to_vec()).unwrap();}
            for workspace in owner.desktop.workspaces.iter().filter(|workspace|workspace.id!=original) {
                if let Some(surface)=workspace.selected_tab().and_then(|id|owner.surfaces.get(&id)) {surface.terminal.input(b"title:Background Agent\r".to_vec()).unwrap();}
            }
        }
        if verify_titles && ticks.get()==40 {if let Some(surface)=controller.borrow().surfaces.get(&initial_tab) {surface.terminal.input(b"title-reset\r".to_vec()).unwrap();}}
        if verify_titles && (42..=45).contains(&ticks.get()) {
            let owner=controller.borrow();
            title_reset_seen.set(owner.desktop.current().base_pane_for_tab(initial_tab).and_then(|id|owner.desktop.current().tree.pane(id)).and_then(|pane|pane.tabs.iter().find(|tab|tab.id==initial_tab)).is_some_and(|tab|tab.display_title()=="Terminal"));
        }
        if verify_titles && ticks.get()==46 {let id=controller.borrow().desktop.workspaces.iter().find(|workspace|workspace.id!=original).unwrap().id;window.invoke_workspace_select(id.to_string().into());}
        if verify_titles && ticks.get()==48 {
            background_title_seen.set(window.get_panes().iter().any(|pane|pane.tabs.iter().any(|tab|tab.title=="Background Agent")));
            if let Some(surface)=controller.borrow().surfaces.get(&initial_tab) {surface.terminal.input(b"title:Restored Console\r".to_vec()).unwrap();}
        }
        if verify_titles && ticks.get()==50 {window.invoke_workspace_select(original.to_string().into());}
        if verify_app_chrome && ticks.get()==51 {window.invoke_show_workspace_menu();}
        if verify_app_chrome && ticks.get()==54 {
            if let Err(error)=save_client_snapshot(&window,&output,"application-menu.png") {chrome_capture_errors.borrow_mut().push(error.to_string());}
            window.invoke_show_palette();
        }
        if verify_app_chrome && ticks.get()==57 {
            if let Err(error)=save_client_snapshot(&window,&output,"command-palette.png") {chrome_capture_errors.borrow_mut().push(error.to_string());}
            window.invoke_show_workspace_menu();
        }
        if ticks.get() < 60 { return; }
        let result = (|| -> Result<(), Box<dyn std::error::Error>> {
            let owner = controller.borrow();
            let marker = owner.surfaces.values().any(|surface| {
                let snapshot = surface.terminal.model().snapshot();
                snapshot.cells.iter().map(|cell| cell.text.as_str()).collect::<String>().matches("BASALT_PORT_PTY_OK").count() >= 2
            });
            let workspace = owner.desktop.current();
            let layout = workspace.tree.layout(owner.viewport, 1.0).ok_or("Layout is invalid")?;
            let dock_panes=owner.desktop.dock.as_ref().map_or(0,|dock|dock.tree.pane_ids().len());
            let dock_marker=owner.desktop.dock.as_ref().is_some_and(|dock|dock.tree.pane_ids().iter().any(|pane| {
                dock.tree.pane(*pane).and_then(|pane|pane.selected).and_then(|id|owner.surfaces.get(&id)).is_some_and(|surface|
                    surface.terminal.model().snapshot().cells.iter().map(|cell|cell.text.as_str()).collect::<String>().matches("BASALT_DOCK_PTY_OK").count()>=2)
            }));
            let canvas_marker=owner.desktop.current().canvas.panes.iter().any(|pane|owner.surfaces.get(&pane.selected).is_some_and(|surface|
                surface.terminal.model().snapshot().cells.iter().map(|cell|cell.text.as_str()).collect::<String>().matches("BASALT_CANVAS_PTY_OK").count()>=2));
            let split_preserved=split_before_canvas.borrow().as_ref().is_some_and(|before|*before==owner.desktop.current().tree.normalized_bounds());
            let canvas_groups=owner.desktop.current().canvas.panes.len();
            let display=window.window().with_winit_window(|native| {
                let monitor=native.current_monitor();
                serde_json::json!({"native_windows_frame":native.is_decorated(),"refresh_millihertz":monitor.as_ref().and_then(|monitor|monitor.refresh_rate_millihertz()),"scale_factor":native.scale_factor(),"monitor_size":monitor.map(|monitor|{let size=monitor.size();[size.width,size.height]})})
            });
            #[cfg(windows)]
            let integrated_frame=window.window().with_winit_window(window_frame::geometry).flatten();
            #[cfg(not(windows))]
            let integrated_frame:Option<serde_json::Value>=None;
            let cell_width=f64::from(window.global::<basalt::TerminalStyle>().get_cell_width());
            let cell_height=f64::from(window.global::<basalt::TerminalStyle>().get_cell_height());
            let visible=owner.desktop.visible_layout(owner.viewport,f64::from(window.window().scale_factor()),owner.dock_width).ok_or("Invalid resized layout")?;
            let grids_fit=visible.panes.iter().all(|frame| {
                let header=f64::from(window.global::<basalt::Metrics>().get_tabbar_height());
                let inner=basalt::viewport::panel_inner_rect(frame.bounds);
                let typography=basalt::typography::PaneTypography {font_size:f64::from(window.global::<basalt::TerminalStyle>().get_font_size()),cell_width,cell_height};
                let wanted=basalt::viewport::terminal_size_with_header(inner,typography.cell_width,typography.cell_height,f64::from(window.window().scale_factor()),header);
                let surface=owner.desktop.owner_of_pane(frame.id).and_then(|workspace|workspace.projected_pane(frame.id)).and_then(|pane|pane.selected).and_then(|id|owner.surfaces.get(&id));
                match (wanted,surface) { (Some(wanted),Some(surface))=>{let snapshot=surface.terminal.model().snapshot();surface.size==wanted && snapshot.columns==usize::from(wanted.columns) && snapshot.rows==usize::from(wanted.rows) && (f64::from(surface.frame.font_size)-typography.font_size).abs()<0.01}, (None,_)=>true,_=>false }
            });
            let focused_size=owner.desktop.focused_container().selected_tab().and_then(|id|owner.surfaces.get(&id)).map(|surface|surface.size);
            let grid_changed=grid_before_resize.borrow().is_some() && *grid_before_resize.borrow()!=focused_size;
            let measured_chrome_fits=window.get_header_height()>=window.get_header_font_height()+7.9 && window.global::<basalt::Metrics>().get_tabbar_height()>=window.get_tab_font_height()+3.9;
            let font_saved=owner.preferences.as_ref().is_some_and(|settings|settings.font_percent().ok()==Some(200));
            let glyph_evidence=owner.surfaces.values().map(|surface|surface.terminal.model().snapshot()).find(|snapshot|snapshot.cells.iter().any(|cell|cell.text=="界"));
            let glyph_anchors=glyph_evidence.as_ref().is_some_and(|snapshot| {
                let frame=screen::frame(snapshot,false);
                snapshot.cells.iter().filter(|cell|cell.flags&64==0 && !cell.text.trim().is_empty()).all(|cell|
                    screen::flat_runs(&frame).iter().any(|run|run.row==cell.line+snapshot.display_offset as i32 && if run.plain_ascii {
                        let offset=cell.column as i32-run.column;
                        offset>=0 && offset<run.cells && run.text.as_bytes().get(offset as usize).is_some_and(|byte|cell.text.as_bytes()==[*byte])
                    } else {run.column==cell.column as i32 && run.text.as_str()==cell.text && run.cells==if cell.flags&32!=0 {2}else{1}}))
            });
            let mixed_clusters=glyph_evidence.as_ref().is_some_and(|snapshot|["界","😀","e\u{301}"].iter().all(|text|snapshot.cells.iter().any(|cell|cell.text==*text)));
            let nerd_clusters=glyph_evidence.as_ref().is_some_and(|snapshot|["\u{f120}","\u{f07b}","\u{f126}"].iter().all(|text|snapshot.cells.iter().any(|cell|cell.text==*text)));
            let title_ui=window.get_panes().iter().any(|pane|pane.tabs.iter().any(|tab|tab.id==initial_tab.to_string() && tab.title=="Restored Console"));
            let interaction_ok = interaction_tab.get().and_then(|id| owner.surfaces.get(&id)).is_some_and(|surface| surface.terminal.title() == "BASALT_POINTER_KEY_OK");
            let mut passed = stage.get() == 2 && (marker || original_marker_seen.get()) && owner.failures.is_empty() && owner.pending.is_empty()
                && (!verify_app_chrome || chrome_capture_errors.borrow().is_empty())
                && (!verify_interaction || (interaction_ok && startup_input_seen.get()))
                && (!verify_titles || (title_reset_seen.get() && background_title_seen.get() && title_ui && owner.desktop.current().title=="My workspace"))
                && (!verify_glyphs || (mixed_clusters && nerd_clusters && glyph_anchors && grids_fit))
                && (!verify_fonts || (window.global::<basalt::Appearance>().get_font_percent()==200 && font_saved && measured_chrome_fits && grids_fit && *grid_before_font.borrow()!=focused_size && integrated_frame.as_ref().is_some_and(|frame|frame["hit_tests"]["lower_header_drag"]==2)))
                && (!verify_dpi || (dpi_event_sent.get() && (window.global::<basalt::Appearance>().get_dpi()-1.5).abs()<0.001 && (window.window().scale_factor()-1.5).abs()<0.001 && grids_fit && *grid_before_dpi.borrow()!=focused_size))
                && (!verify_dock || (dock_panes==3 && dock_marker && owner.desktop.dock_focused && window.get_dock_panes().row_count()==3))
                && (!verify_canvas || ((canvas_marker || canvas_marker_seen.get()) && split_preserved && canvas_groups==14 && window.get_panes().row_count()==14 && window.get_canvas_mode()))
                && (!verify_resize || (grids_fit && grid_changed))
                && owner.desktop.selected == original && owner.desktop.workspaces.len() == 2 && layout.panes.len() == 12 && layout.dividers.len() == 11
                && integrated_frame.as_ref().is_some_and(|frame|frame["subclass_installed"]==true && frame["client_top_inset"]==0 && frame["hit_tests"]["toolbar"]==2 && frame["hit_tests"]["drag"]==2 && frame["hit_tests"]["minimize"]==8 && frame["hit_tests"]["maximize"]==9 && frame["hit_tests"]["close"]==20);
            fs::create_dir_all(&output)?;
            #[cfg(windows)]
            if let Some(result)=window.window().with_winit_window(|native|window_frame::capture(native,&output.join("whole-window.png"))) {result?;}
            let buffer = window.window().take_snapshot()?;
            let body_start = ((window.get_header_height() * window.window().scale_factor()).ceil() as usize * buffer.width() as usize * 4).min(buffer.as_bytes().len());
            let client_opaque = buffer.as_bytes()[body_start..].chunks_exact(4).all(|pixel|pixel[3] == 255);
            passed &= !verify_interaction || client_opaque;
            let scale=window.window().scale_factor();
            let title_left=((window.get_sidebar_width()+32.0)*scale).round() as u32;
            let title_right=(title_left+(160.0*scale) as u32).min(buffer.width());
            let title_bottom=((window.get_header_height()*scale) as u32).min(buffer.height());
            let mut shades=[false;256];
            for y in 0..title_bottom {for x in title_left..title_right {
                let offset=((y*buffer.width()+x)*4) as usize;let rgb=&buffer.as_bytes()[offset..offset+3];
                if (46..=200).contains(&rgb[0]) && rgb[2]>=rgb[0] && rgb[2].abs_diff(rgb[0])<60 {shades[rgb[0] as usize]=true;}
            }}
            let text_edge_shades=shades.iter().filter(|shade|**shade).count();
            let logical_title_size=window.global::<basalt::Appearance>().invoke_text_size(12.5);
            passed &= !verify_typography || ((logical_title_size-12.5).abs()<0.001 && text_edge_shades>=8);
            let mut encoder = png::Encoder::new(BufWriter::new(File::create(output.join("desktop.png"))?), buffer.width(), buffer.height());
            encoder.set_color(png::ColorType::Rgba); encoder.set_depth(png::BitDepth::Eight);
            encoder.write_header()?.write_image_data(buffer.as_bytes())?;
            let test_details=serde_json::json!({
                "passed": passed, "pty_marker": marker, "workspace_count": owner.desktop.workspaces.len(),
                "pane_count": layout.panes.len(), "divider_count": layout.dividers.len(), "live_surfaces": owner.surfaces.len(),
                "dock_test_enabled":verify_dock,"dock_pane_count":dock_panes,"dock_pty_marker":dock_marker,"dock_focused":owner.desktop.dock_focused,
                "canvas_test_enabled":verify_canvas,"canvas_pty_marker":canvas_marker,"canvas_split_tree_preserved":split_preserved,"canvas_group_count":canvas_groups,
                "context_input_via_ui_key_callback":verify_canvas||verify_dock,"canvas_pty_marker_observed":canvas_marker_seen.get(),
                "resize_test_enabled":verify_resize,"actual_pty_grids_fit_pane_bodies":grids_fit,"pty_grid_changed_after_resize":grid_changed,
                "grid_before_resize":*grid_before_resize.borrow(),"grid_after_resize":focused_size,"logical_viewport":[owner.viewport.width,owner.viewport.height],
                "dpi_test_enabled":verify_dpi,"native_dpi_event_sent":dpi_event_sent.get(),"appearance_dpi":window.global::<basalt::Appearance>().get_dpi(),
                "grid_before_dpi":*grid_before_dpi.borrow(),"dpi_test_kind":"owned-window WM_DPICHANGED simulation; not physical monitor transfer",
                "typography_test_enabled":verify_typography,"logical_sidebar_title_size":logical_title_size,"native_text_edge_gray_levels":text_edge_shades,
                "snapshot_resampled":false,"glyph_rendering":"Skia native text with installed JetBrainsMono NFM; logical font size preserved",
            });
            let mut report=serde_json::json!({
                "app_chrome_capture_enabled":verify_app_chrome,"app_chrome_capture_errors":*chrome_capture_errors.borrow(),
                "pointer_and_key_dispatch_enabled":verify_interaction,"pointer_and_key_reached_clicked_pty":interaction_ok,"startup_keys_reached_original_pty":startup_input_seen.get(),"client_body_backdrop_fully_opaque":client_opaque,
                "per_pane_font_sizes":window.get_panes().iter().map(|pane|pane.frame.font_size).collect::<Vec<_>>(),
                "nerd_font_clusters_received_over_pty":nerd_clusters,"terminal_font_family":window.global::<basalt::TerminalStyle>().get_font_family().to_string(),
                "title_test_enabled":verify_titles,"title_reset_fallback_seen":title_reset_seen.get(),"background_title_projected_on_select":background_title_seen.get(),"restored_title_in_ui":title_ui,
                "glyph_test_enabled":verify_glyphs,"mixed_clusters_received_over_pty":mixed_clusters,"native_cell_anchors_preserved":glyph_anchors,
                "font_magnification_test_enabled":verify_fonts,"font_magnification_percent":window.global::<basalt::Appearance>().get_font_percent(),
                "font_preference_saved":font_saved,"measured_chrome_fits_text":measured_chrome_fits,"header_height":window.get_header_height(),
                "tabbar_height":window.global::<basalt::Metrics>().get_tabbar_height(),"grid_before_font":*grid_before_font.borrow(),
                "pending_spawns": owner.pending.len(), "failures": owner.failures.values().collect::<Vec<_>>(),
                "snapshot_width": buffer.width(), "snapshot_height": buffer.height(),
                "text_renderer":"Skia native text; system fonts and anti-aliasing",
                "tab_drag_transfer_roundtrip":stage.get()==2,
                "display":display,
                "integrated_frame":integrated_frame,
                "terminal_poll_microseconds":5000,
                "fixture_trace":*trace.borrow(),
                "physical_test_input_isolated":true,
                "scope": "Native Slint window, PTY output, tabs, nested splits, workspace navigation"
            });
            if let serde_json::Value::Object(details)=test_details {report.as_object_mut().unwrap().extend(details);}
            fs::write(output.join("desktop-smoke.json"), serde_json::to_vec_pretty(&report)?)?;
            if !passed { return Err("Native UI smoke failed; inspect desktop-smoke.json".into()); }
            if output.join("desktop-smoke-error.txt").is_file() { fs::remove_file(output.join("desktop-smoke-error.txt"))?; }
            Ok(())
        })();
        result_flag.set(result.is_ok());
        if let Err(error) = result { let _ = fs::create_dir_all(&output); let _ = fs::write(output.join("desktop-smoke-error.txt"), error.to_string()); }
        let _ = slint::quit_event_loop();
    });
    SmokeRun {
        _timer: timer,
        passed,
    }
}

fn save_client_snapshot(
    window: &MainWindow,
    output: &std::path::Path,
    name: &str,
) -> Result<(), Box<dyn std::error::Error>> {
    fs::create_dir_all(output)?;
    let buffer = window.window().take_snapshot()?;
    let mut encoder = png::Encoder::new(
        BufWriter::new(File::create(output.join(name))?),
        buffer.width(),
        buffer.height(),
    );
    encoder.set_color(png::ColorType::Rgba);
    encoder.set_depth(png::BitDepth::Eight);
    encoder
        .write_header()?
        .write_image_data(buffer.as_bytes())?;
    Ok(())
}

fn fixture(owner: &Controller, step: &str) -> serde_json::Value {
    let workspace = owner.desktop.current();
    serde_json::json!({"step":step,"workspace":workspace.id.to_string(),"focused":workspace.focused.to_string(),"panes":workspace.tree.pane_ids().iter().map(|id|{let pane=workspace.tree.pane(*id).unwrap();serde_json::json!({"id":id.to_string(),"tabs":pane.tabs.iter().map(|tab|tab.id.to_string()).collect::<Vec<_>>()})}).collect::<Vec<_>>()})
}

fn marker_present(surface: &Surface, marker: &str) -> bool {
    surface
        .terminal
        .model()
        .snapshot()
        .cells
        .iter()
        .map(|cell| cell.text.as_str())
        .collect::<String>()
        .matches(marker)
        .count()
        >= 2
}

pub fn workspace_tools(
    window: &MainWindow,
    controller: Rc<RefCell<Controller>>,
    output: PathBuf,
) -> SmokeRun {
    let timer = slint::Timer::default();
    let weak = window.as_weak();
    let stage = Cell::new(0);
    let ticks = Cell::new(0);
    let source = Cell::new(Uuid::nil());
    let other = Cell::new(Uuid::nil());
    let target = Cell::new(Uuid::nil());
    let pid = Cell::new(0);
    let passed = Rc::new(Cell::new(false));
    let flag = passed.clone();
    window
        .window()
        .set_size(slint::LogicalSize::new(1200.0, 800.0));
    timer.start(slint::TimerMode::Repeated,Duration::from_millis(250),move || {
        let Some(window)=weak.upgrade() else{return;};ticks.set(ticks.get()+1);
        let result=(||->Result<(),Box<dyn std::error::Error>> {
            fs::create_dir_all(&output)?;

            if stage.get()==0 {
                if !controller.borrow().pending.is_empty()||controller.borrow().surfaces.is_empty(){return Ok(());}
                {let owner=controller.borrow();source.set(owner.desktop.selected);let tab=owner.desktop.current().selected_tab().unwrap();target.set(tab);pid.set(owner.surfaces[&tab].terminal.pid());}
                window.invoke_color_change(1);window.invoke_font_change(120);window.invoke_terminal_font_change(140);window.invoke_chrome_action(2);other.set(controller.borrow().desktop.selected);window.invoke_color_change(4);window.invoke_font_change(90);
                window.invoke_workspace_select(source.get().to_string().into());
                if window.global::<basalt::Theme>().get_palette()!=4 || window.global::<basalt::Appearance>().get_font_percent()!=90 || window.global::<basalt::Appearance>().get_terminal_font_percent()!=140 {return Err("Global appearance changed on workspace selection".into());}
                if controller.borrow().surfaces[&target.get()].terminal.pid()!=pid.get(){return Err("Appearance switch restarted terminal".into());}
                stage.set(10);return Ok(());
            }
            if stage.get()==10 {
                for state_name in ["working","waiting","completed","error"] {
                    let mut owner=controller.borrow_mut();owner.tab_tools.insert(target.get(),"claude".into());owner.signals.insert(target.get(),basalt::agent_bridge::Signal{provider:"claude".into(),state:state_name.into(),session_id:"fixture".into(),turn_id:"fixture".into(),event:"fixture".into()});owner.refresh_agents(&window);drop(owner);
                    save_client_snapshot(&window,&output,&format!("agents-{state_name}.png"))?;
                }
                if std::env::args().any(|arg|arg=="--verify-design") {
                    let saved_font=window.global::<basalt::Appearance>().get_font_percent();
                    for percent in [150,200] {window.invoke_font_change(percent);save_client_snapshot(&window,&output,&format!("appearance-{percent}.png"))?;}
                    window.invoke_font_change(saved_font);
                }
                {let mut owner=controller.borrow_mut();owner.tab_tools.remove(&target.get());owner.signals.remove(&target.get());owner.refresh_agents(&window);}
                controller.borrow_mut().clipboard.write("echo CLIPBOARD_MAIN_OK\r\n".into())?;
                window.invoke_key_input("v".into(),true,false,false,false);stage.set(1);return Ok(());
            }
            if stage.get()==1 {
                if !marker_present(&controller.borrow().surfaces[&target.get()],"CLIPBOARD_MAIN_OK"){return Ok(());}
                window.invoke_key_input("c".into(),true,true,false,false);
                if !controller.borrow().clipboard.read()?.contains("CLIPBOARD_MAIN_OK"){return Err("Copy did not capture terminal output".into());}
                window.invoke_workspace_select(other.get().to_string().into());
                let terminal_size=window.global::<basalt::TerminalStyle>().get_font_size();
                window.invoke_font_change(110);
                if window.global::<basalt::TerminalStyle>().get_font_size()!=terminal_size {return Err("Interface font changed terminal size".into());}
                window.invoke_terminal_font_change(150);
                if window.global::<basalt::Appearance>().get_font_percent()!=110 || window.global::<basalt::TerminalStyle>().get_font_size()==terminal_size {return Err("Independent terminal font failed".into());}
                stage.set(2);return Ok(());
            }
            if stage.get()==2 {
                window.invoke_reset_workspace_appearance();
                if window.global::<basalt::Theme>().get_palette()!=controller.borrow().default_palette {return Err("Appearance inheritance reset failed".into());}
                fs::write(output.join("workspace-tools.json"),serde_json::to_vec_pretty(&serde_json::json!({"passed":true,"clipboard_main":true,"global_appearance_and_font_independence":true,"scope":"Owned CMD and isolated clipboard; no credentials/model calls"}))?)?;flag.set(true);let _=slint::quit_event_loop();stage.set(9);
            }
            Ok(())
        })();
        if let Err(error)=result{let _=fs::write(output.join("tools-error.txt"),error.to_string());let _=slint::quit_event_loop();}
        if ticks.get()>140 {let _=fs::write(output.join("tools-error.txt"),format!("Timed out at stage {}",stage.get()));let _=slint::quit_event_loop();}
    });
    SmokeRun {
        _timer: timer,
        passed,
    }
}
