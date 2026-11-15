use super::parse_str;

#[test]
fn parse_comment() {
    parse_str(
        "//
        ////
        ///////////////////////////////////////////////////////// asda
        //////////////////// dad ////////// /
        /////////////////////////////////////////////////////////////////////////////////////////////////////
        //
    ",
    )
    .unwrap();
}

#[test]
fn parse_types() {
    parse_str("const a : i32 = 2;").unwrap();
    parse_str("const a : u64 = 2lu;").unwrap();
    assert!(parse_str("const a : x32 = 2;").is_err());
    parse_str("var t: texture_2d<f32>;").unwrap();
    parse_str("var t: texture_cube_array<i32>;").unwrap();
    parse_str("var t: texture_multisampled_2d<u32>;").unwrap();
    parse_str("var t: texture_storage_1d<rgba8uint,write>;").unwrap();
    parse_str("var t: texture_storage_3d<r32float,read>;").unwrap();
}

#[test]
fn parse_type_inference() {
    parse_str(
        "
        fn foo() {
            let a = 2u;
            let b: u32 = a;
            var x = 3.;
            var y = vec2<f32>(1, 2);
        }",
    )
    .unwrap();
    assert!(parse_str(
        "
        fn foo() { let c : i32 = 2.0; }",
    )
    .is_err());
}

#[test]
fn parse_type_cast() {
    parse_str(
        "
        const a : i32 = 2;
        fn main() {
            var x: f32 = f32(a);
            x = f32(i32(a + 1) / 2);
        }
    ",
    )
    .unwrap();
    parse_str(
        "
        fn main() {
            let x: vec2<f32> = vec2<f32>(1.0, 2.0);
            let y: vec2<u32> = vec2<u32>(x);
        }
    ",
    )
    .unwrap();
    parse_str(
        "
        fn main() {
            let x: vec2<f32> = vec2<f32>(0.0);
        }
    ",
    )
    .unwrap();
    assert!(parse_str(
        "
        fn main() {
            let x: vec2<f32> = vec2<f32>(0i, 0i);
        }
    ",
    )
    .is_err());
}

#[test]
fn parse_type_coercion() {
    parse_str(
        "
        fn foo(bar: f32) {}
        fn main() {
            foo(0);
        }
    ",
    )
    .unwrap();
    assert!(parse_str(
        "
        fn foo(bar: i32) {}
        fn main() {
            foo(0.0);
        }
    ",
    )
    .is_err());
}

#[test]
fn parse_struct() {
    parse_str(
        "
        struct Foo { x: i32 }
        struct Bar {
            @size(16) x: vec2<i32>,
            @align(16) y: f32,
            @size(32) @align(128) z: vec3<f32>,
        };
        struct Empty {}
        var<storage,read_write> s: Foo;
    ",
    )
    .unwrap();
}

#[test]
fn parse_standard_fun() {
    parse_str(
        "
        fn main() {
            var x: i32 = min(max(1, 2), 3);
        }
    ",
    )
    .unwrap();
}

#[test]
fn parse_statement() {
    parse_str(
        "
        fn main() {
            ;
            {}
            {;}
        }
    ",
    )
    .unwrap();

    parse_str(
        "
        fn foo() {}
        fn bar() { foo(); }
    ",
    )
    .unwrap();
}

#[test]
fn parse_if() {
    parse_str(
        "
        fn main() {
            if true {
                discard;
            } else {}
            if 0 != 1 {}
            if false {
                return;
            } else if true {
                return;
            } else {}
        }
    ",
    )
    .unwrap();
}

#[test]
fn parse_parentheses_if() {
    parse_str(
        "
        fn main() {
            if (true) {
                discard;
            } else {}
            if (0 != 1) {}
            if (false) {
                return;
            } else if (true) {
                return;
            } else {}
        }
    ",
    )
    .unwrap();
}

#[test]
fn parse_loop() {
    parse_str(
        "
        fn main() {
            var i: i32 = 0;
            loop {
                if i == 1 { break; }
                continuing { i = 1; }
            }
            loop {
                if i == 0 { continue; }
                break;
            }
        }
    ",
    )
    .unwrap();
    parse_str(
        "
        fn main() {
            var found: bool = false;
            var i: i32 = 0;
            while !found {
                if i == 10 {
                    found = true;
                }

                i = i + 1;
            }
        }
    ",
    )
    .unwrap();
    parse_str(
        "
        fn main() {
            while true {
                break;
            }
        }
    ",
    )
    .unwrap();
    parse_str(
        "
        fn main() {
            var a: i32 = 0;
            for(var i: i32 = 0; i < 4; i = i + 1) {
                a = a + 2;
            }
        }
    ",
    )
    .unwrap();
    parse_str(
        "
        fn main() {
            for(;;) {
                break;
            }
        }
    ",
    )
    .unwrap();
}

#[test]
fn parse_switch() {
    parse_str(
        "
        fn main() {
            var pos: f32;
            switch (3) {
                case 0, 1: { pos = 0.0; }
                case 2: { pos = 1.0; }
                default: { pos = 3.0; }
            }
        }
    ",
    )
    .unwrap();
}

#[test]
fn parse_switch_optional_colon_in_case() {
    parse_str(
        "
        fn main() {
            var pos: f32;
            switch (3) {
                case 0, 1 { pos = 0.0; }
                case 2 { pos = 1.0; }
                default { pos = 3.0; }
            }
        }
    ",
    )
    .unwrap();
}

#[test]
fn parse_switch_default_in_case() {
    parse_str(
        "
        fn main() {
            var pos: f32;
            switch (3) {
                case 0, 1: { pos = 0.0; }
                case 2: {}
                case default, 3: { pos = 3.0; }
            }
        }
    ",
    )
    .unwrap();
}

#[test]
fn parse_parentheses_switch() {
    parse_str(
        "
        fn main() {
            var pos: f32;
            switch pos > 1.0 {
                default: { pos = 3.0; }
            }
        }
    ",
    )
    .unwrap();
}

#[test]
fn parse_texture_load() {
    parse_str(
        "
        var t: texture_3d<u32>;
        fn foo() {
            let r: vec4<u32> = textureLoad(t, vec3<u32>(0u, 1u, 2u), 1);
        }
    ",
    )
    .unwrap();
    parse_str(
        "
        var t: texture_multisampled_2d_array<i32>;
        fn foo() {
            let r: vec4<i32> = textureLoad(t, vec2<i32>(10, 20), 2, 3);
        }
    ",
    )
    .unwrap();
    parse_str(
        "
        var t: texture_storage_1d_array<r32float,read>;
        fn foo() {
            let r: vec4<f32> = textureLoad(t, 10, 2);
        }
    ",
    )
    .unwrap();
}

#[test]
fn parse_texture_store() {
    parse_str(
        "
        var t: texture_storage_2d<rgba8unorm,write>;
        fn foo() {
            textureStore(t, vec2<i32>(10, 20), vec4<f32>(0.0, 1.0, 2.0, 3.0));
        }
    ",
    )
    .unwrap();
}

#[test]
fn parse_texture_query() {
    parse_str(
        "
        var t: texture_multisampled_2d_array<f32>;
        fn foo() {
            var dim: vec2<u32> = textureDimensions(t);
            dim = textureDimensions(t, 0);
            let layers: u32 = textureNumLayers(t);
            let samples: u32 = textureNumSamples(t);
        }
    ",
    )
    .unwrap();
}

#[test]
fn parse_postfix() {
    parse_str(
        "fn foo() {
        let x: f32 = vec4<f32>(1.0, 2.0, 3.0, 4.0).xyz.rgbr.aaaa.wz.g;
        let y: f32 = fract(vec2<f32>(0.5, x)).x;
    }",
    )
    .unwrap();

    let err = parse_str(
        "fn foo() {
        let v = mat4x4<f32>().x;
    }",
    )
    .unwrap_err();
    assert_eq!(err.message(), "invalid field accessor `x`");
}

#[test]
fn parse_expressions() {
    parse_str("fn foo() {
        let x: f32 = select(0.0, 1.0, true);
        let y: vec2<f32> = select(vec2<f32>(1.0, 1.0), vec2<f32>(x, x), vec2<bool>(x < 0.5, x > 0.5));
        let z: bool = !(0.0 == 1.0);
    }").unwrap();
}

#[test]
fn binary_expression_mixed_scalar_and_vector_operands() {
    for (operand, expect_splat) in [
        ('<', false),
        ('>', false),
        ('&', false),
        ('|', false),
        ('+', true),
        ('-', true),
        ('*', false),
        ('/', true),
        ('%', true),
    ] {
        let module = parse_str(&format!(
            "
            @fragment
            fn main(@location(0) some_vec: vec3<f32>) -> @location(0) vec4<f32> {{
                if (all(1.0 {operand} some_vec)) {{
                    return vec4(0.0);
                }}
                return vec4(1.0);
            }}
            "
        ))
        .unwrap();

        let expressions = &&module.entry_points[0].function.expressions;

        let found_expressions = expressions
            .iter()
            .filter(|&(_, e)| {
                if let crate::Expression::Binary { left, .. } = *e {
                    matches!(
                        (expect_splat, &expressions[left]),
                        (false, &crate::Expression::Literal(crate::Literal::F32(..)))
                            | (true, &crate::Expression::Splat { .. })
                    )
                } else {
                    false
                }
            })
            .count();

        assert_eq!(
            found_expressions,
            1,
            "expected `{operand}` expression {} splat",
            if expect_splat { "with" } else { "without" }
        );
    }

    let module = parse_str(
        "@fragment
        fn main(mat: mat3x3<f32>) {
            let vec = vec3<f32>(1.0, 1.0, 1.0);
            let result = mat / vec;
        }",
    )
    .unwrap();
    let expressions = &&module.entry_points[0].function.expressions;
    let found_splat = expressions.iter().any(|(_, e)| {
        if let crate::Expression::Binary { left, .. } = *e {
            matches!(&expressions[left], &crate::Expression::Splat { .. })
        } else {
            false
        }
    });
    assert!(!found_splat, "'mat / vec' should not be splatted");
}

#[test]
fn parse_pointers() {
    parse_str(
        "fn foo(a: ptr<function, f32>) -> f32 { return *a; }
    fn bar() {
        var x: f32 = 1.0;
        let px = &x;
        let py = foo(px);
    }",
    )
    .unwrap();
}

#[test]
fn parse_struct_instantiation() {
    parse_str(
        "
    struct Foo {
        a: f32,
        b: vec3<f32>,
    }

    @fragment
    fn fs_main() {
        var foo: Foo = Foo(0.0, vec3<f32>(0.0, 1.0, 42.0));
    }
    ",
    )
    .unwrap();
}

#[test]
fn parse_array_length() {
    parse_str(
        "
        struct Foo {
            data: array<u32>
        } // this is used as both input and output for convenience

        @group(0) @binding(0)
        var<storage> foo: Foo;

        @group(0) @binding(1)
        var<storage> bar: array<u32>;

        fn baz() {
            var x: u32 = arrayLength(foo.data);
            var y: u32 = arrayLength(bar);
        }
        ",
    )
    .unwrap();
}

#[test]
fn parse_storage_buffers() {
    parse_str(
        "
        @group(0) @binding(0)
        var<storage> foo: array<u32>;
        ",
    )
    .unwrap();
    parse_str(
        "
        @group(0) @binding(0)
        var<storage,read> foo: array<u32>;
        ",
    )
    .unwrap();
    parse_str(
        "
        @group(0) @binding(0)
        var<storage,write> foo: array<u32>;
        ",
    )
    .unwrap();
    parse_str(
        "
        @group(0) @binding(0)
        var<storage,read_write> foo: array<u32>;
        ",
    )
    .unwrap();
}

#[test]
fn parse_alias() {
    parse_str(
        "
        alias Vec4 = vec4<f32>;
        ",
    )
    .unwrap();
}

#[test]
fn parse_texture_load_store_expecting_four_args() {
    for (func, texture) in [
        (
            "textureStore",
            "texture_storage_2d_array<rg11b10float, write>",
        ),
        ("textureLoad", "texture_2d_array<i32>"),
    ] {
        let error = parse_str(&format!(
            "
            @group(0) @binding(0) var tex_los_res: {texture};
            @compute
            @workgroup_size(1)
            fn main(@builtin(global_invocation_id) id: vec3<u32>) {{
                var color = vec4(1, 1, 1, 1);
                {func}(tex_los_res, id, color);
            }}
            "
        ))
        .unwrap_err();
        assert_eq!(
            error.message(),
            "wrong number of arguments: expected 4, found 3"
        );
    }
}

#[test]
fn parse_repeated_attributes() {
    use crate::{
        front::wgsl::{error::Error, Frontend},
        Span,
    };

    let template_vs = "@vertex fn vs() -> __REPLACE__ vec4<f32> { return vec4<f32>(0.0); }";
    let template_struct = "struct A { __REPLACE__ data: vec3<f32> }";
    let template_resource = "__REPLACE__ var tex_los_res: texture_2d_array<i32>;";
    let template_stage = "__REPLACE__ fn vs() -> vec4<f32> { return vec4<f32>(0.0); }";
    for (attribute, template) in [
        ("align(16)", template_struct),
        ("binding(0)", template_resource),
        ("builtin(position)", template_vs),
        ("compute", template_stage),
        ("fragment", template_stage),
        ("group(0)", template_resource),
        ("interpolate(flat)", template_vs),
        ("invariant", template_vs),
        ("location(0)", template_vs),
        ("size(16)", template_struct),
        ("vertex", template_stage),
        ("early_depth_test(less_equal)", template_resource),
        ("workgroup_size(1)", template_stage),
    ] {
        let shader = template.replace("__REPLACE__", &format!("@{attribute} @{attribute}"));
        let name_length = attribute.rfind('(').unwrap_or(attribute.len()) as u32;
        let span_start = shader.rfind(attribute).unwrap() as u32;
        let span_end = span_start + name_length;
        let expected_span = Span::new(span_start, span_end);

        let result = Frontend::new().inner(&shader);
        assert!(matches!(
            result.unwrap_err(),
            Error::RepeatedAttribute(span) if span == expected_span
        ));
    }
}

#[test]
fn parse_missing_workgroup_size() {
    use crate::{
        front::wgsl::{error::Error, Frontend},
        Span,
    };

    let shader = "@compute fn vs() -> vec4<f32> { return vec4<f32>(0.0); }";
    let result = Frontend::new().inner(shader);
    assert!(matches!(
        result.unwrap_err(),
        Error::MissingWorkgroupSize(span) if span == Span::new(1, 8)
    ));
}

mod diagnostic_filter {
    use crate::front::wgsl::assert_parse_err;

    #[test]
    fn intended_global_directive() {
        let shader = "@diagnostic(off, my.lint);";
        assert_parse_err(
            shader,
            "\
error: `@diagnostic(…)` attribute(s) on semicolons are not supported
  ┌─ wgsl:1:1
  │
1 │ @diagnostic(off, my.lint);
  │ ^^^^^^^^^^^^^^^^^^^^^^^^^
  │
  = note: `@diagnostic(…)` attributes are only permitted on `fn`s, some statements, and `switch`/`loop` bodies.
  = note: If you meant to declare a diagnostic filter that applies to the entire module, move this line to the top of the file and remove the `@` symbol.

"
        );
    }

    mod parse_sites_not_yet_supported {
        use crate::front::wgsl::assert_parse_err;

        #[test]
        fn user_rules() {
            let shader = "
fn myfunc() {
    if (true) @diagnostic(off, my.lint) {
        //    ^^^^^^^^^^^^^^^^^^^^^^^^^ not yet supported, should report an error
    }
}
";
            assert_parse_err(shader, "\
error: `@diagnostic(…)` attribute(s) not yet implemented
  ┌─ wgsl:3:15
  │
3 │     if (true) @diagnostic(off, my.lint) {
  │               ^^^^^^^^^^^^^^^^^^^^^^^^^ can't use this on compound statements (yet)
  │
  = note: Let Naga maintainers know that you ran into this at <https://github.com/gfx-rs/wgpu/issues/5320>, so they can prioritize it!

");
        }

        #[test]
        fn unknown_rules() {
            let shader = "
fn myfunc() {
	if (true) @diagnostic(off, wat_is_this) {
		//    ^^^^^^^^^^^^^^^^^^^^^^^^^^^^^ should emit a warning
	}
}
";
            assert_parse_err(shader, "\
error: `@diagnostic(…)` attribute(s) not yet implemented
  ┌─ wgsl:3:12
  │
3 │     if (true) @diagnostic(off, wat_is_this) {
  │               ^^^^^^^^^^^^^^^^^^^^^^^^^^^^^ can't use this on compound statements (yet)
  │
  = note: Let Naga maintainers know that you ran into this at <https://github.com/gfx-rs/wgpu/issues/5320>, so they can prioritize it!

");
        }
    }

    mod directive_conflict {
        use crate::front::wgsl::assert_parse_err;

        #[test]
        fn user_rules() {
            let shader = "
diagnostic(off, my.lint);
diagnostic(warning, my.lint);
";
            assert_parse_err(shader, "\
error: found conflicting `diagnostic(…)` rule(s)
  ┌─ wgsl:2:1
  │
2 │ diagnostic(off, my.lint);
  │ ^^^^^^^^^^^^^^^^^^^^^^^^ first rule
3 │ diagnostic(warning, my.lint);
  │ ^^^^^^^^^^^^^^^^^^^^^^^^^^^^ second rule
  │
  = note: Multiple `diagnostic(…)` rules with the same rule name conflict unless they are directives and the severity is the same.
  = note: You should delete the rule you don't want.

");
        }

        #[test]
        fn unknown_rules() {
            let shader = "
diagnostic(off, wat_is_this);
diagnostic(warning, wat_is_this);
";
            assert_parse_err(shader, "\
error: found conflicting `diagnostic(…)` rule(s)
  ┌─ wgsl:2:1
  │
2 │ diagnostic(off, wat_is_this);
  │ ^^^^^^^^^^^^^^^^^^^^^^^^^^^^ first rule
3 │ diagnostic(warning, wat_is_this);
  │ ^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^ second rule
  │
  = note: Multiple `diagnostic(…)` rules with the same rule name conflict unless they are directives and the severity is the same.
  = note: You should delete the rule you don't want.

");
        }
    }

    mod attribute_conflict {
        use crate::front::wgsl::assert_parse_err;

        #[test]
        fn user_rules() {
            let shader = "
diagnostic(off, my.lint);
diagnostic(warning, my.lint);
";
            assert_parse_err(shader, "\
error: found conflicting `diagnostic(…)` rule(s)
  ┌─ wgsl:2:1
  │
2 │ diagnostic(off, my.lint);
  │ ^^^^^^^^^^^^^^^^^^^^^^^^ first rule
3 │ diagnostic(warning, my.lint);
  │ ^^^^^^^^^^^^^^^^^^^^^^^^^^^^ second rule
  │
  = note: Multiple `diagnostic(…)` rules with the same rule name conflict unless they are directives and the severity is the same.
  = note: You should delete the rule you don't want.

");
        }

        #[test]
        fn unknown_rules() {
            let shader = "
diagnostic(off, wat_is_this);
diagnostic(warning, wat_is_this);
";
            assert_parse_err(shader, "\
error: found conflicting `diagnostic(…)` rule(s)
  ┌─ wgsl:2:1
  │
2 │ diagnostic(off, wat_is_this);
  │ ^^^^^^^^^^^^^^^^^^^^^^^^^^^^ first rule
3 │ diagnostic(warning, wat_is_this);
  │ ^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^ second rule
  │
  = note: Multiple `diagnostic(…)` rules with the same rule name conflict unless they are directives and the severity is the same.
  = note: You should delete the rule you don't want.

");
        }
    }
}

const BASE_CLAMP_SHADER: &str = r#"
    @group(0) @binding(0) var t: texture_2d<f32>;
    @group(0) @binding(1) var s: sampler;
    fn sample_base(coords: vec2<f32>) -> vec4<f32> {
        return textureSampleBaseClampToEdge(t, s, coords);
    }
    @fragment fn main(@location(0) coords: vec2<f32>) -> @location(0) vec4<f32> {
        return sample_base(coords);
    }
"#;

#[test]
fn texture_sample_base_clamp_to_edge_lowers_half_texel_bounds() {
    let module = parse_str(BASE_CLAMP_SHADER).unwrap();
    crate::valid::Validator::new(
        crate::valid::ValidationFlags::all(),
        crate::valid::Capabilities::empty(),
    )
    .validate(&module)
    .unwrap();
    let function = module
        .functions
        .iter()
        .find(|(_, f)| f.name.as_deref() == Some("sample_base"))
        .unwrap()
        .1;
    let (image, coordinate) = function
        .expressions
        .iter()
        .find_map(|(_, e)| match *e {
            crate::Expression::ImageSample {
                image,
                coordinate,
                level: crate::SampleLevel::Zero,
                gather: None,
                depth_ref: None,
                array_index: None,
                offset: None,
                ..
            } => Some((image, coordinate)),
            _ => None,
        })
        .expect("base-level sample with no extra operands");
    assert!(function.expressions.iter().any(|(_, e)| matches!(e,
        crate::Expression::ImageQuery { image: query_image, query: crate::ImageQuery::Size { level: None } }
            if *query_image == image)));

    // Evaluate the lowered coordinate expression with runtime dimensions.
    // Non-square and one-texel images expose an incorrect scalar extent or
    // [0, 1] clamp, even when ordinary in-range samples look correct.
    fn evaluate(
        expressions: &crate::Arena<crate::Expression>,
        handle: crate::Handle<crate::Expression>,
        coords: [f32; 2],
        dimensions: [f32; 2],
    ) -> [f32; 2] {
        use crate::{BinaryOperator as B, Expression as E, MathFunction as M};
        let eval = |h| evaluate(expressions, h, coords, dimensions);
        match expressions[handle] {
            E::FunctionArgument(0) => coords,
            E::Literal(crate::Literal::F32(value)) => [value; 2],
            E::Splat {
                size: crate::VectorSize::Bi,
                value,
            } => eval(value),
            E::Compose { ref components, .. } if components.len() == 2 => {
                [eval(components[0])[0], eval(components[1])[0]]
            }
            E::ImageQuery {
                query: crate::ImageQuery::Size { level: None },
                ..
            } => dimensions,
            E::As {
                expr,
                kind: crate::ScalarKind::Float,
                convert: Some(4),
            } => eval(expr),
            E::Binary { op, left, right } => {
                let (left, right) = (eval(left), eval(right));
                core::array::from_fn(|i| match op {
                    B::Divide => left[i] / right[i],
                    B::Subtract => left[i] - right[i],
                    _ => panic!("unexpected coordinate arithmetic: {op:?}"),
                })
            }
            E::Math {
                fun: M::Clamp,
                arg,
                arg1: Some(lo),
                arg2: Some(hi),
                arg3: None,
            } => {
                let (value, lo, hi) = (eval(arg), eval(lo), eval(hi));
                core::array::from_fn(|i| value[i].clamp(lo[i], hi[i]))
            }
            ref other => panic!("unexpected coordinate expression: {other:?}"),
        }
    }
    for (coords, dimensions, expected) in [
        ([-3.0, 5.0], [8.0, 4.0], [0.0625, 0.875]),
        ([0.25, 0.5], [8.0, 4.0], [0.25, 0.5]),
        ([0.0, 1.0], [1.0, 32.0], [0.5, 0.984375]),
        ([1.0, 0.0], [16.0, 1.0], [0.96875, 0.5]),
    ] {
        assert_eq!(
            evaluate(&function.expressions, coordinate, coords, dimensions),
            expected
        );
    }

    // Base-level sampling requires no derivatives and is valid outside fragment shaders.
    let declarations = BASE_CLAMP_SHADER.split("@fragment").next().unwrap();
    for entry in [
        "@vertex fn main() -> @builtin(position) vec4<f32> { return sample_base(vec2(0.5)); }",
        "@group(0) @binding(2) var<storage, read_write> output: vec4<f32>;\n@compute @workgroup_size(1) fn main() { output = sample_base(vec2(0.5)); }",
    ] {
        let module = parse_str(&format!("{declarations}{entry}")).unwrap();
        crate::valid::Validator::new(crate::valid::ValidationFlags::all(), crate::valid::Capabilities::empty())
            .validate(&module).unwrap();
    }
}

#[test]
fn texture_sample_base_clamp_to_edge_rejects_other_overloads() {
    for texture in [
        "texture_1d<f32>",
        "texture_2d_array<f32>",
        "texture_3d<f32>",
        "texture_cube<f32>",
        "texture_depth_2d",
        "texture_2d<i32>",
        "texture_2d<u32>",
        "texture_multisampled_2d<f32>",
        "texture_storage_2d<rgba8unorm, read>",
    ] {
        let source = BASE_CLAMP_SHADER.replace("texture_2d<f32>", texture);
        let error = parse_str(&source).expect_err(texture);
        assert!(
            error
                .message()
                .contains("requires (texture_2d<f32>, sampler, vec2<f32>)"),
            "{texture}: {error:?}"
        );
    }
    for replacement in [
        "t, s, 0.5",
        "t, s, vec3<f32>(0.5)",
        "t, s, vec2<i32>(1)",
        "t, s, vec2<u32>(1)",
        "t, s",
        "t, s, coords, vec2<i32>(0)",
    ] {
        let source = BASE_CLAMP_SHADER.replace(
            "textureSampleBaseClampToEdge(t, s, coords)",
            &format!("textureSampleBaseClampToEdge({replacement})"),
        );
        assert!(parse_str(&source).is_err(), "accepted {replacement}");
    }
    assert!(
        parse_str(&BASE_CLAMP_SHADER.replace("var s: sampler;", "var s: sampler_comparison;"))
            .is_err()
    );
}

#[test]
#[cfg(feature = "spv-out")]
fn texture_sample_base_clamp_to_edge_spirv_has_clamp_and_explicit_zero_lod() {
    let module = parse_str(BASE_CLAMP_SHADER).unwrap();
    let info = crate::valid::Validator::new(
        crate::valid::ValidationFlags::all(),
        crate::valid::Capabilities::empty(),
    )
    .validate(&module)
    .unwrap();
    let words = crate::back::spv::write_vec(
        &module,
        &info,
        &crate::back::spv::Options::default(),
        Some(&crate::back::spv::PipelineOptions {
            shader_stage: crate::ShaderStage::Fragment,
            entry_point: "main".into(),
        }),
    )
    .unwrap();
    let mut instructions = Vec::new();
    let mut cursor = 5;
    while cursor < words.len() {
        let count = (words[cursor] >> 16) as usize;
        assert!(count > 0);
        instructions.push(&words[cursor..cursor + count]);
        cursor += count;
    }
    let is_op = |i: &&[u32], op: spirv::Op| i[0] & 0xffff == op as u32;
    let zero = |id| {
        instructions
            .iter()
            .any(|i| is_op(i, spirv::Op::Constant) && i[2] == id && i[3] == 0)
    };
    let samples: Vec<_> = instructions
        .iter()
        .filter(|i| is_op(i, spirv::Op::ImageSampleExplicitLod))
        .collect();
    assert_eq!(samples.len(), 1);
    assert_eq!(samples[0][5], spirv::ImageOperands::LOD.bits());
    assert!(zero(samples[0][6]));
    assert!(!instructions
        .iter()
        .any(|i| is_op(i, spirv::Op::ImageSampleImplicitLod)));
    let queries: Vec<_> = instructions
        .iter()
        .filter(|i| is_op(i, spirv::Op::ImageQuerySizeLod))
        .collect();
    assert_eq!(queries.len(), 1);
    assert!(zero(queries[0][4]));
    assert!(instructions
        .iter()
        .any(|i| is_op(i, spirv::Op::ExtInst) && i[4] == spirv::GLOp::FClamp as u32));
}
