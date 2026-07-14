use sutra::tools::help;

#[test]
fn no_topic_returns_topic_list() {
    let result = help::handle(None).unwrap();
    let topics = result["topics"].as_array().unwrap();
    assert!(!topics.is_empty());
    for topic in topics {
        assert!(topic["name"].is_string());
        assert!(topic["summary"].is_string());
    }
}

#[test]
fn valid_topic_returns_focused_content() {
    let result = help::handle(Some("quickstart")).unwrap();
    assert_eq!(result["topic"].as_str().unwrap(), "quickstart");
    let content = result["content"].as_str().unwrap();
    assert!(content.contains("sutra_workspace"));
    assert!(content.contains("sutra_map"));
}

#[test]
fn unknown_topic_returns_available_list() {
    let result = help::handle(Some("nonexistent")).unwrap();
    assert!(result["error"].as_str().unwrap().contains("nonexistent"));
    assert!(result["available_topics"].is_array());
}

#[test]
fn recipes_topic_has_at_least_five_recipes() {
    let result = help::handle(Some("recipes")).unwrap();
    let content = result["content"].as_str().unwrap();
    let recipe_count = content.matches("## ").count();
    assert!(
        recipe_count >= 5,
        "expected at least 5 recipes, found {recipe_count}"
    );
}

#[test]
fn recipes_reference_sutra_review() {
    let result = help::handle(Some("recipes")).unwrap();
    let content = result["content"].as_str().unwrap();
    assert!(
        content.contains("sutra_review"),
        "recipes should reference sutra_review"
    );
}

#[test]
fn all_topics_are_reachable() {
    let index = help::handle(None).unwrap();
    let topics = index["topics"].as_array().unwrap();
    for topic in topics {
        let name = topic["name"].as_str().unwrap();
        let result = help::handle(Some(name)).unwrap();
        assert!(
            result["content"].is_string(),
            "topic '{name}' should return content"
        );
    }
}
