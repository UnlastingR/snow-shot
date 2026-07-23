pub(crate) fn project_size_to_aspect(raw_width: f32, raw_height: f32, aspect: f32) -> (f32, f32) {
    let aspect = sanitize_aspect(aspect);
    let denominator = aspect * aspect + 1.0;
    let projected_height = (raw_width * aspect + raw_height) / denominator;
    (projected_height * aspect, projected_height)
}

pub(crate) fn proportional_scale_from_delta(
    delta_width: f32,
    delta_height: f32,
    base_width: f32,
    base_height: f32,
) -> f32 {
    let denominator = base_width * base_width + base_height * base_height;
    if !denominator.is_finite() || denominator <= f32::EPSILON {
        return 1.0;
    }

    1.0 + (delta_width * base_width + delta_height * base_height) / denominator
}

fn sanitize_aspect(aspect: f32) -> f32 {
    if aspect.is_finite() && aspect > f32::EPSILON {
        aspect
    } else {
        1.0
    }
}

#[cfg(test)]
mod tests {
    use super::{project_size_to_aspect, proportional_scale_from_delta};

    #[test]
    fn aspect_projection_is_continuous_across_the_diagonal() {
        let before = project_size_to_aspect(80.0, 39.0, 2.0);
        let on_diagonal = project_size_to_aspect(80.0, 40.0, 2.0);
        let after = project_size_to_aspect(80.0, 41.0, 2.0);

        let first_step = on_diagonal.0 - before.0;
        let second_step = after.0 - on_diagonal.0;
        assert!((first_step - second_step).abs() < 0.0001);
        assert!((on_diagonal.0 / on_diagonal.1 - 2.0).abs() < 0.0001);
    }

    #[test]
    fn proportional_delta_projection_is_continuous_across_the_diagonal() {
        let before = proportional_scale_from_delta(80.0, 39.0, 400.0, 200.0);
        let on_diagonal = proportional_scale_from_delta(80.0, 40.0, 400.0, 200.0);
        let after = proportional_scale_from_delta(80.0, 41.0, 400.0, 200.0);

        let first_step = on_diagonal - before;
        let second_step = after - on_diagonal;
        assert!((first_step - second_step).abs() < 0.0001);
        assert!((on_diagonal - 1.2).abs() < 0.0001);
    }
}
