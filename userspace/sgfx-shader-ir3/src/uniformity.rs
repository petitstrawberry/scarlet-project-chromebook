// SPDX-License-Identifier: GPL-2.0-only
//! Workgroup convergence analysis over *validated* Naga IR. Naga 24 deliberately
//! omits barrier checks and its expression uniformity treats all mutable locals
//! as divergent. Build value/control dependencies instead, including cyclic loop
//! phis, then solve reachability from the non-uniform source. No source matching.
//!
//! Conservative: aggregates are one cell, storage/shared/private reads are varying,
//! and algebraic equivalence/termination are not proved. A uniform-loop induction
//! variable stays uniform; a value becoming varying on a later iteration does not.
use crate::Error;
use naga::{AddressSpace, Binding, BuiltIn, Expression as E, Function, Handle, Statement as S};
use std::collections::{BTreeMap, VecDeque};

type Node = usize;
const UNIFORM: Node = 0;
const VARYING: Node = 1;
const BUDGET: usize = 100_000;
type State = BTreeMap<usize, Node>;

#[derive(Clone, Copy)]
enum Place {
    Local { cell: usize, partial: bool },
    Global(AddressSpace),
}
#[derive(Clone)]
struct Value {
    node: Node,
    place: Option<Place>,
    fields: Vec<Value>,
}
impl Value {
    fn scalar(node: Node) -> Self {
        Self {
            node,
            place: None,
            fields: Vec::new(),
        }
    }
}
#[derive(Clone)]
struct Path {
    state: State,
    pc: Node,
}
#[derive(Default)]
struct Flow {
    next: Option<Path>,
    breaks: Vec<Path>,
    continues: Vec<Path>,
    returns: Vec<(Path, Node)>,
    // Explicit non-returning paths (loops with no reachable break).
    stalls: Vec<Node>,
}
impl Flow {
    fn exits(&self) -> impl Iterator<Item = Node> + '_ {
        self.breaks
            .iter()
            .chain(&self.continues)
            .map(|p| p.pc)
            .chain(self.returns.iter().map(|(p, _)| p.pc))
            .chain(self.stalls.iter().copied())
    }
    fn append(&mut self, mut other: Self) {
        self.breaks.append(&mut other.breaks);
        self.continues.append(&mut other.continues);
        self.returns.append(&mut other.returns);
        self.stalls.append(&mut other.stalls);
    }
}
struct Frame<'a> {
    function: &'a Function,
    arguments: Vec<Value>,
    locals: Vec<usize>,
    expressions: Vec<Option<Value>>,
}
struct Analysis<'a> {
    module: &'a naga::Module,
    edges: Vec<Vec<Node>>,
    requirements: Vec<(Node, String)>,
    cells: usize,
    calls: usize,
    steps: usize,
    exhausted: bool,
}
impl<'a> Analysis<'a> {
    fn node(&mut self, deps: impl IntoIterator<Item = Node>) -> Node {
        if self.edges.len() >= BUDGET {
            self.exhausted = true;
            return VARYING;
        }
        let id = self.edges.len();
        self.edges.push(deps.into_iter().collect());
        id
    }
    fn merge(&mut self, paths: &[Path], pc: Node) -> Option<Path> {
        if paths.is_empty() {
            return None;
        }
        let mut values: BTreeMap<usize, Vec<Node>> = BTreeMap::new();
        for path in paths {
            for (&cell, &value) in &path.state {
                values.entry(cell).or_default().push(value);
            }
        }
        let state = values
            .into_iter()
            .map(|(cell, mut deps)| {
                deps.sort_unstable();
                deps.dedup();
                let node = if deps.len() == 1 {
                    deps[0]
                } else {
                    self.node(deps)
                };
                (cell, node)
            })
            .collect();
        Some(Path { state, pc })
    }
    fn entry_argument(&mut self, ty: Handle<naga::Type>, binding: &Option<Binding>) -> Value {
        if let Some(Binding::BuiltIn(
            BuiltIn::WorkGroupId | BuiltIn::WorkGroupSize | BuiltIn::NumWorkGroups,
        )) = binding
        {
            return Value::scalar(UNIFORM);
        }
        if let naga::TypeInner::Struct { ref members, .. } = self.module.types[ty].inner {
            let fields = members
                .iter()
                .map(|m| self.entry_argument(m.ty, &m.binding))
                .collect::<Vec<_>>();
            let node = self.node(fields.iter().map(|v| v.node));
            Value {
                node,
                place: None,
                fields,
            }
        } else {
            Value::scalar(VARYING)
        }
    }
    fn expression(&mut self, frame: &mut Frame<'a>, state: &State, handle: Handle<E>) -> Value {
        if let Some(value) = &frame.expressions[handle.index()] {
            return value.clone();
        }
        let value = match &frame.function.expressions[handle] {
            E::Literal(_) | E::Constant(_) | E::Override(_) | E::ZeroValue(_) => {
                Value::scalar(UNIFORM)
            }
            E::FunctionArgument(index) => frame.arguments[*index as usize].clone(),
            E::LocalVariable(local) => Value {
                node: UNIFORM,
                place: Some(Place::Local {
                    cell: frame.locals[local.index()],
                    partial: false,
                }),
                fields: Vec::new(),
            },
            E::GlobalVariable(global) => Value {
                node: UNIFORM,
                place: Some(Place::Global(self.module.global_variables[*global].space)),
                fields: Vec::new(),
            },
            E::Access { base, index } => {
                let mut base = self.expression(frame, state, *base);
                let index = self.expression(frame, state, *index);
                base.node = self.node([base.node, index.node]);
                if let Some(Place::Local { cell, .. }) = base.place {
                    base.place = Some(Place::Local {
                        cell,
                        partial: true,
                    });
                }
                base.fields.clear();
                base
            }
            E::AccessIndex { base, index } => {
                let mut base = self.expression(frame, state, *base);
                if let Some(field) = base.fields.get(*index as usize) {
                    field.clone()
                } else {
                    if let Some(Place::Local { cell, .. }) = base.place {
                        base.place = Some(Place::Local {
                            cell,
                            partial: true,
                        });
                    }
                    base.fields.clear();
                    base
                }
            }
            E::Load { pointer } => {
                let pointer = self.expression(frame, state, *pointer);
                let contents = match pointer.place {
                    Some(Place::Local { cell, .. }) => state.get(&cell).copied().unwrap_or(VARYING),
                    Some(Place::Global(AddressSpace::Uniform | AddressSpace::PushConstant)) => {
                        UNIFORM
                    }
                    // Even a uniform workgroup address is not a uniform load.
                    _ => VARYING,
                };
                Value::scalar(self.node([pointer.node, contents]))
            }
            E::Compose { ty, components } => {
                let fields = components
                    .iter()
                    .map(|h| self.expression(frame, state, *h))
                    .collect::<Vec<_>>();
                let node = self.node(fields.iter().map(|v| v.node));
                // Vector constructor operands are not vector components:
                // vec4(varying_vec3, 0) has TWO operands but FOUR components.
                // Only structs have a guaranteed one-to-one member mapping.
                let fields =
                    if matches!(self.module.types[*ty].inner, naga::TypeInner::Struct { .. }) {
                        fields
                    } else {
                        Vec::new()
                    };
                Value {
                    node,
                    place: None,
                    fields,
                }
            }
            E::Splat { value, .. }
            | E::Unary { expr: value, .. }
            | E::As { expr: value, .. }
            | E::Relational {
                argument: value, ..
            }
            | E::Swizzle { vector: value, .. } => {
                let node = self.expression(frame, state, *value).node;
                Value::scalar(node)
            }
            E::Binary { left, right, .. } => {
                let left = self.expression(frame, state, *left).node;
                let right = self.expression(frame, state, *right).node;
                Value::scalar(self.node([left, right]))
            }
            E::Select {
                condition,
                accept,
                reject,
            } => {
                let deps =
                    [*condition, *accept, *reject].map(|h| self.expression(frame, state, h).node);
                Value::scalar(self.node(deps))
            }
            E::Math {
                arg,
                arg1,
                arg2,
                arg3,
                ..
            } => {
                let deps = [Some(*arg), *arg1, *arg2, *arg3]
                    .into_iter()
                    .flatten()
                    .map(|h| self.expression(frame, state, h).node)
                    .collect::<Vec<_>>();
                Value::scalar(self.node(deps))
            }
            E::ArrayLength(pointer) => {
                let node = self.expression(frame, state, *pointer).node;
                Value::scalar(node)
            }
            E::ImageQuery { image, query } => {
                let image = self.expression(frame, state, *image).node;
                let level = match query {
                    naga::ImageQuery::Size { level: Some(level) } => {
                        self.expression(frame, state, *level).node
                    }
                    _ => UNIFORM,
                };
                Value::scalar(self.node([image, level]))
            }
            // Calls/uniform loads set their result at the statement. Unset is
            // conservative. Texture, atomic and subgroup values are not proven
            // uniform across the entire workgroup (which can span many waves).
            _ => Value::scalar(VARYING),
        };
        frame.expressions[handle.index()] = Some(value.clone());
        value
    }
    fn function(
        &mut self,
        function: &'a Function,
        arguments: Vec<Value>,
        mut path: Path,
        depth: usize,
    ) -> Result<(Option<Path>, Node, Vec<Node>), Error> {
        self.calls += 1;
        if depth > 64 || self.calls > 4096 {
            return Err(Error(
                "workgroup uniformity analysis budget exceeded".into(),
            ));
        }
        let mut frame = Frame {
            function,
            arguments,
            locals: Vec::new(),
            expressions: vec![None; function.expressions.len()],
        };
        for (_, local) in function.local_variables.iter() {
            let cell = self.cells;
            self.cells += 1;
            frame.locals.push(cell);
            let initial = local
                .init
                .map(|h| self.expression(&mut frame, &path.state, h).node)
                .unwrap_or(UNIFORM);
            path.state.insert(cell, initial);
        }
        let pc = path.pc;
        let flow = self.block(&mut frame, &function.body, path, depth)?;
        let result = self.node(flow.returns.iter().map(|(_, value)| *value));
        let mut paths = flow.returns.into_iter().map(|(p, _)| p).collect::<Vec<_>>();
        paths.extend(flow.next);
        let pc = self.node(std::iter::once(pc).chain(flow.stalls.iter().copied()));
        Ok((self.merge(&paths, pc), result, flow.stalls))
    }
    fn block(
        &mut self,
        frame: &mut Frame<'a>,
        block: &naga::Block,
        path: Path,
        depth: usize,
    ) -> Result<Flow, Error> {
        let mut flow = Flow {
            next: Some(path),
            ..Default::default()
        };
        for statement in block.iter() {
            let Some(mut path) = flow.next.take() else {
                break;
            };
            self.steps += 1;
            if self.steps > BUDGET || self.exhausted {
                return Err(Error(
                    "workgroup uniformity analysis budget exceeded".into(),
                ));
            }
            let mut out = Flow::default();
            match statement {
                S::Emit(range) => {
                    for h in range.clone() {
                        self.expression(frame, &path.state, h);
                    }
                }
                S::Store { pointer, value } => {
                    let pointer = self.expression(frame, &path.state, *pointer);
                    let value = self.expression(frame, &path.state, *value);
                    if let Some(Place::Local { cell, partial }) = pointer.place {
                        let previous = if partial {
                            path.state.get(&cell).copied().unwrap_or(VARYING)
                        } else {
                            UNIFORM
                        };
                        let value = self.node([path.pc, pointer.node, value.node, previous]);
                        path.state.insert(cell, value);
                    } else if pointer.place.is_none() {
                        // A future/imported pointer expression may have lost its
                        // precise alias target. Never silently ignore that write.
                        // Taint all tracked cells rather than prove a false uniform.
                        for value in path.state.values_mut() {
                            *value = VARYING;
                        }
                    }
                }
                S::Barrier(_) => self.requirements.push((
                    path.pc,
                    format!(
                        "barrier in {}",
                        frame.function.name.as_deref().unwrap_or("entry")
                    ),
                )),
                S::WorkGroupUniformLoad { pointer, result } => {
                    let pointer = self.expression(frame, &path.state, *pointer);
                    let required = self.node([path.pc, pointer.node]);
                    self.requirements.push((
                        required,
                        format!(
                            "workgroupUniformLoad control flow or pointer in {}",
                            frame.function.name.as_deref().unwrap_or("entry")
                        ),
                    ));
                    frame.expressions[result.index()] = Some(Value::scalar(UNIFORM));
                }
                S::Block(block) => {
                    out = self.block(frame, block, path, depth)?;
                    flow.next = out.next.take();
                    flow.append(out);
                    continue;
                }
                S::If {
                    condition,
                    accept,
                    reject,
                } => {
                    let condition = self.expression(frame, &path.state, *condition).node;
                    let pc = self.node([path.pc, condition]);
                    let mut yes = self.block(
                        frame,
                        accept,
                        Path {
                            state: path.state.clone(),
                            pc,
                        },
                        depth,
                    )?;
                    let mut no = self.block(
                        frame,
                        reject,
                        Path {
                            state: path.state,
                            pc,
                        },
                        depth,
                    )?;
                    let exit_pc = self.node(
                        std::iter::once(path.pc)
                            .chain(yes.exits())
                            .chain(no.exits()),
                    );
                    let paths = yes
                        .next
                        .take()
                        .into_iter()
                        .chain(no.next.take())
                        .collect::<Vec<_>>();
                    // No abrupt exit: both arms reconverge, regardless of selector.
                    out.next = self.merge(&paths, exit_pc);
                    out.append(yes);
                    out.append(no);
                    flow.next = out.next.take();
                    flow.append(out);
                    continue;
                }
                S::Switch { selector, cases } => {
                    let selector = self.expression(frame, &path.state, *selector).node;
                    let branch_pc = self.node([path.pc, selector]);
                    let mut fallthrough = None;
                    let mut joins = Vec::new();
                    for case in cases {
                        let mut starts = vec![Path {
                            state: path.state.clone(),
                            pc: branch_pc,
                        }];
                        starts.extend(fallthrough.take());
                        let start_pc = self.node(starts.iter().map(|p| p.pc));
                        let start = self.merge(&starts, start_pc).unwrap();
                        let mut branch = self.block(frame, &case.body, start, depth)?;
                        joins.append(&mut branch.breaks); // switch breaks reconverge here
                        if case.fall_through {
                            fallthrough = branch.next.take();
                        } else {
                            joins.extend(branch.next.take());
                        }
                        out.append(branch);
                    }
                    joins.extend(fallthrough);
                    let pc = self.node(std::iter::once(path.pc).chain(out.exits()));
                    out.next = self.merge(&joins, pc);
                    flow.next = out.next.take();
                    flow.append(out);
                    continue;
                }
                S::Loop {
                    body,
                    continuing,
                    break_if,
                } => {
                    let header = self.node([path.pc]);
                    let phis = path
                        .state
                        .iter()
                        .map(|(&cell, &initial)| (cell, self.node([initial])))
                        .collect::<State>();
                    let mut body_flow = self.block(
                        frame,
                        body,
                        Path {
                            state: phis.clone(),
                            pc: header,
                        },
                        depth,
                    )?;
                    // Any divergent abrupt exit can change participation on a
                    // subsequent iteration, including an exit *after* a barrier.
                    let exits = body_flow.exits().collect::<Vec<_>>();
                    self.edges[header].extend(exits);
                    let mut backs = std::mem::take(&mut body_flow.continues);
                    backs.extend(body_flow.next.take());
                    let back_pc =
                        self.node(std::iter::once(header).chain(backs.iter().map(|p| p.pc)));
                    let mut has_backedge = false;
                    if let Some(back) = self.merge(&backs, back_pc) {
                        let mut continued = self.block(frame, continuing, back, depth)?;
                        if let Some(mut back) = continued.next.take() {
                            has_backedge = true;
                            if let Some(condition) = break_if {
                                let condition =
                                    self.expression(frame, &back.state, *condition).node;
                                back.pc = self.node([back.pc, condition]);
                                body_flow.breaks.push(back.clone());
                            }
                            self.edges[header].push(back.pc);
                            for (&cell, &phi) in &phis {
                                if let Some(value) = back.state.get(&cell) {
                                    self.edges[phi].push(*value);
                                }
                            }
                        }
                        body_flow.append(continued);
                    }
                    if has_backedge && body_flow.breaks.is_empty() {
                        body_flow.stalls.push(header);
                    }
                    // Loop breaks reconverge at the loop exit. Returns/stalls do not.
                    let exit_pc = self.node(
                        std::iter::once(path.pc)
                            .chain(body_flow.returns.iter().map(|(p, _)| p.pc))
                            .chain(body_flow.stalls.iter().copied()),
                    );
                    out.next = self.merge(&body_flow.breaks, exit_pc);
                    out.returns = body_flow.returns;
                    out.stalls = body_flow.stalls;
                    flow.next = out.next.take();
                    flow.append(out);
                    continue;
                }
                S::Break => {
                    flow.breaks.push(path);
                    continue;
                }
                S::Continue => {
                    flow.continues.push(path);
                    continue;
                }
                S::Return { value } => {
                    let value = value
                        .map(|h| self.expression(frame, &path.state, h).node)
                        .unwrap_or(UNIFORM);
                    let value = self.node([path.pc, value]);
                    flow.returns.push((path, value));
                    continue;
                }
                S::Call {
                    function,
                    arguments,
                    result,
                } => {
                    let arguments = arguments
                        .iter()
                        .map(|h| self.expression(frame, &path.state, *h))
                        .collect();
                    let (next, value, stalls) = self.function(
                        &self.module.functions[*function],
                        arguments,
                        path,
                        depth + 1,
                    )?;
                    if let Some(result) = result {
                        frame.expressions[result.index()] = Some(Value::scalar(value));
                    }
                    flow.next = next;
                    flow.stalls.extend(stalls);
                    continue;
                }
                S::Atomic { result, .. } => {
                    if let Some(result) = result {
                        frame.expressions[result.index()] = Some(Value::scalar(VARYING));
                    }
                }
                S::ImageStore { .. } | S::ImageAtomic { .. } => {}
                // Outside this compiler's compute profile; never assume new
                // control-flow/memory constructs preserve workgroup convergence.
                _ => {
                    return Err(Error(
                        "unsupported statement in workgroup uniformity analysis".into(),
                    ));
                }
            }
            flow.next = Some(path);
        }
        Ok(flow)
    }
    fn finish(&self, entry: &str) -> Result<(), Error> {
        if self.exhausted {
            return Err(Error(
                "workgroup uniformity analysis budget exceeded".into(),
            ));
        }
        let mut reverse = vec![Vec::new(); self.edges.len()];
        for (node, deps) in self.edges.iter().enumerate() {
            for &dep in deps {
                reverse[dep].push(node);
            }
        }
        let mut varying = vec![false; self.edges.len()];
        let mut queue = VecDeque::from([VARYING]);
        varying[VARYING] = true;
        while let Some(node) = queue.pop_front() {
            for &dependent in &reverse[node] {
                if !varying[dependent] {
                    varying[dependent] = true;
                    queue.push_back(dependent);
                }
            }
        }
        for (node, operation) in &self.requirements {
            if varying[*node] {
                return Err(Error(format!(
                    "workgroup uniformity: non-uniform {operation} (entry {entry})"
                )));
            }
        }
        Ok(())
    }
}

/// Call only after Naga's full semantic validation, including stage/type/scope
/// checks and recursion rejection. Apply to every compute entry for validate().
pub(crate) fn validate(
    module: &naga::Module,
    _info: &naga::valid::ModuleInfo,
) -> Result<(), Error> {
    for entry in &module.entry_points {
        if entry.stage != naga::ShaderStage::Compute {
            continue;
        }
        let mut analysis = Analysis {
            module,
            edges: vec![vec![], vec![]],
            requirements: Vec::new(),
            cells: 0,
            calls: 0,
            steps: 0,
            exhausted: false,
        };
        let args = entry
            .function
            .arguments
            .iter()
            .map(|a| analysis.entry_argument(a.ty, &a.binding))
            .collect();
        analysis.function(
            &entry.function,
            args,
            Path {
                state: State::new(),
                pc: UNIFORM,
            },
            0,
        )?;
        analysis.finish(&entry.name)?;
    }
    Ok(())
}
