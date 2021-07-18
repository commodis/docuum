use crate::{
    format::CodeStr,
    state::{self, State},
    Settings,
};
use byte_unit::Byte;
use chrono::DateTime;
use regex::RegexSet;
use scopeguard::guard;
use serde::{Deserialize, Serialize};
use std::{
    cmp::max,
    collections::{HashMap, HashSet},
    io::{self, BufRead, BufReader},
    mem::drop,
    process::{Command, Stdio},
    time::{Duration, SystemTime, UNIX_EPOCH},
};

// A Docker event (a line of output from `docker events --format '{{json .}}'`)
#[derive(Deserialize, Serialize, Debug)]
struct Event {
    #[serde(rename = "Type")]
    r#type: String,

    #[serde(rename = "Action")]
    action: String,

    #[serde(rename = "Actor")]
    actor: EventActor,

    id: String,
}

// A Docker event actor
#[derive(Deserialize, Serialize, Debug)]
struct EventActor {
    #[serde(rename = "Attributes")]
    attributes: EventActorAttributes,
}

// Docker event actor attributes
#[derive(Deserialize, Serialize, Debug)]
struct EventActorAttributes {
    image: Option<String>,
}

// A line of output from `docker system df --format '{{json .}}'`
#[derive(Deserialize, Serialize, Debug)]
struct SpaceRecord {
    #[serde(rename = "Type")]
    r#type: String,

    #[serde(rename = "Size")]
    size: String,
}

// Each image may be associated with multiple of these repository-tag pairs.
#[derive(Clone, Debug, Eq, PartialEq)]
struct RepositoryTag {
    repository: String,
    tag: String,
}

// The information we get about an image from Docker
#[derive(Clone, Debug, Eq, PartialEq)]
struct ImageRecord {
    id: String,
    parent_id: Option<String>,
    created_since_epoch: Duration,
    repository_tag: RepositoryTag,
}

// A node in the image polyforest
#[derive(Clone, Debug, Eq, PartialEq)]
struct Image {
    image_record: ImageRecord,
    last_used_since_epoch: Duration,
    ancestors: usize, // 0 for images with no parent or missing parent
}

// Ask Docker for the ID of an image.
fn image_id(image: &str) -> io::Result<String> {
    // Query Docker for the image ID.
    let output = Command::new("docker")
        .args(&["image", "inspect", "--format", "{{.ID}}", image])
        .stderr(Stdio::inherit())
        .output()?;

    // Ensure the command succeeded.
    if !output.status.success() {
        return Err(io::Error::new(
            io::ErrorKind::Other,
            format!("Unable to determine ID of image {}.", image.code_str()),
        ));
    }

    // Interpret the output bytes as UTF-8 and trim any leading/trailing whitespace.
    String::from_utf8(output.stdout)
        .map(|output| output.trim().to_owned())
        .map_err(|error| io::Error::new(io::ErrorKind::Other, error))
}

// Ask Docker for the ID of the parent of an image.
fn parent_id(image_id: &str) -> io::Result<Option<String>> {
    // Query Docker for the parent image ID.
    let output = Command::new("docker")
        .args(&["image", "inspect", "--format", "{{.Parent}}", image_id])
        .stderr(Stdio::inherit())
        .output()?;

    // Ensure the command succeeded.
    if !output.status.success() {
        return Err(io::Error::new(
            io::ErrorKind::Other,
            format!(
                "Unable to determine ID of the parent of image {}.",
                image_id.code_str(),
            ),
        ));
    }

    // Interpret the output bytes as UTF-8 and trim any leading/trailing whitespace.
    String::from_utf8(output.stdout)
        .map(|output| {
            let trimmed_output = output.trim();

            // Does the image even have a parent?
            if trimmed_output.is_empty() {
                None
            } else {
                Some(trimmed_output.to_owned())
            }
        })
        .map_err(|error| io::Error::new(io::ErrorKind::Other, error))
}

// Query Docker for all the images.
fn list_image_records(state: &mut State) -> io::Result<Vec<ImageRecord>> {
    // Get the IDs and creation timestamps of all the images.
    let output = Command::new("docker")
        .args(&[
            "image",
            "ls",
            "--all",
            "--no-trunc",
            "--format",
            "{{.ID}}\\t{{.Repository}}\\t{{.Tag}}\\t{{.CreatedAt}}",
        ])
        .stderr(Stdio::inherit())
        .output()?;

    // Ensure the command succeeded.
    if !output.status.success() {
        return Err(io::Error::new(
            io::ErrorKind::Other,
            "Unable to determine IDs of all images.",
        ));
    }

    // Interpret the output bytes as UTF-8 and parse the lines.
    let mut image_records = String::from_utf8(output.stdout)
        .map_err(|error| io::Error::new(io::ErrorKind::Other, error))
        .and_then(|output| {
            let mut images = vec![];
            let lines = output.lines();
            for line in lines {
                let trimmed_line = line.trim();

                if trimmed_line.is_empty() {
                    continue;
                }

                let image_parts = trimmed_line.split('\t').collect::<Vec<_>>();
                if let [id, repository, tag, date_str] = image_parts[..] {
                    images.push(ImageRecord {
                        id: id.trim().to_owned(),
                        parent_id: None, // This will be populated below.
                        created_since_epoch: parse_docker_date(date_str)?,
                        repository_tag: RepositoryTag {
                            repository: repository.to_owned(),
                            tag: tag.to_owned(),
                        },
                    });
                } else {
                    return Err(io::Error::new(
                        io::ErrorKind::Other,
                        "Failed to parse image list output from Docker.",
                    ));
                }
            }
            Ok(images)
        })?;

    // Find the parent of each image, either by looking it up in the state or by querying Docker.
    for image_record in &mut image_records {
        if let Some(image) = state.images.get(&image_record.id) {
            image_record.parent_id = image.parent_id.clone();
        } else {
            image_record.parent_id = parent_id(&image_record.id)?;
        };
    }

    Ok(image_records)
}

// Parse the non-standard timestamp format Docker uses for `docker image ls`.
// Example input: "2017-12-20 16:30:49 -0500 EST".
fn parse_docker_date(timestamp: &str) -> io::Result<Duration> {
    // Chrono can't read the "EST", so remove it before parsing.
    let timestamp_without_timezone_triad =
        timestamp.trim().rsplitn(2, ' ').last().ok_or_else(|| {
            io::Error::new(io::ErrorKind::Other, "Failed to remove timezone string.")
        })?;

    // Parse the date and convert it into a duration since the UNIX epoch.
    let duration =
        match DateTime::parse_from_str(&timestamp_without_timezone_triad, "%Y-%m-%d %H:%M:%S %z") {
            Ok(datetime) => {
                datetime.signed_duration_since::<chrono::offset::Utc>(DateTime::from(UNIX_EPOCH))
            }
            Err(error) => return Err(io::Error::new(io::ErrorKind::Other, error)),
        };

    // Convert the duration into a `std::time::Duration`.
    match duration.to_std() {
        Ok(duration) => Ok(duration),
        Err(error) => Err(io::Error::new(io::ErrorKind::Other, error)),
    }
}

// Ask Docker for the IDs of the images currently in use by containers.
fn image_ids_in_use() -> io::Result<HashSet<String>> {
    // Query Docker for the image IDs.
    let output = Command::new("docker")
        .args(&[
            "container",
            "ls",
            "--all",
            "--no-trunc",
            "--format",
            "{{.Image}}",
        ])
        .stderr(Stdio::inherit())
        .output()?;

    // Ensure the command succeeded.
    if !output.status.success() {
        return Err(io::Error::new(
            io::ErrorKind::Other,
            "Unable to determine IDs of images currently in use by containers.",
        ));
    }

    // Interpret the output bytes as UTF-8 and collect the lines.
    String::from_utf8(output.stdout)
        .map_err(|error| io::Error::new(io::ErrorKind::Other, error))
        .map(|output| {
            output
                .lines()
                .filter_map(|line| {
                    if line.is_empty() {
                        None
                    } else {
                        match image_id(line.trim()) {
                            Ok(the_image_id) => Some(the_image_id),
                            Err(error) => {
                                // This failure may happen if a container was created from an image
                                // that no longer exists. This is non-fatal, so we just log the
                                // error and continue.
                                error!("{}", error);
                                None
                            }
                        }
                    }
                })
                .collect()
        })
}

// Get the total space used by Docker images.
#[allow(clippy::map_err_ignore)]
fn space_usage() -> io::Result<Byte> {
    // Query Docker for the space usage.
    let output = Command::new("docker")
        .args(&["system", "df", "--format", "{{json .}}"])
        .stderr(Stdio::inherit())
        .output()?;

    // Ensure the command succeeded.
    if !output.status.success() {
        return Err(io::Error::new(
            io::ErrorKind::Other,
            "Unable to determine the disk space used by Docker images.",
        ));
    }

    // Find the relevant line of output.
    String::from_utf8(output.stdout)
        .map_err(|error| io::Error::new(io::ErrorKind::Other, error))
        .and_then(|output| {
            for line in output.lines() {
                // Parse the line as a space record.
                if let Ok(space_record) = serde_json::from_str::<SpaceRecord>(&line) {
                    // Return early if we found the record we're looking for.
                    if space_record.r#type == "Images" {
                        return Byte::from_str(&space_record.size).map_err(|_| {
                            io::Error::new(
                                io::ErrorKind::Other,
                                format!(
                                    "Unable to parse {} from {}.",
                                    space_record.size.code_str(),
                                    "docker system df".code_str(),
                                ),
                            )
                        });
                    }
                }
            }

            Err(io::Error::new(
                io::ErrorKind::Other,
                format!(
                    "Unable to parse output of {}: {}",
                    "docker system df".code_str(),
                    output.code_str(),
                ),
            ))
        })
}

// Delete a Docker image.
fn delete_image(image_id: &str) -> io::Result<()> {
    info!("Deleting image {}\u{2026}", image_id.code_str());

    // Tell Docker to delete the image.
    let mut child = Command::new("docker")
        .args(&["image", "rm", "--force", "--no-prune", image_id])
        .spawn()?;

    // Ensure the command succeeded.
    if !child.wait()?.success() {
        return Err(io::Error::new(
            io::ErrorKind::Other,
            format!("Unable to delete image {}.", image_id.code_str()),
        ));
    }

    Ok(())
}

// Update the timestamp for an image.
fn touch_image(state: &mut State, image_id: &str, verbose: bool) -> io::Result<()> {
    if verbose {
        info!(
            "Updating last-used timestamp for image {}\u{2026}",
            image_id.code_str(),
        );
    } else {
        debug!(
            "Updating last-used timestamp for image {}\u{2026}",
            image_id.code_str(),
        );
    }

    // Find the parent of the image, either by looking it up in the state or by querying Docker.
    let parent_id = if let Some(image) = state.images.get(image_id) {
        image.parent_id.clone()
    } else {
        parent_id(image_id)?
    };

    // Get the current timestamp.
    match SystemTime::now().duration_since(UNIX_EPOCH) {
        Ok(duration) => {
            // Store the image metadata in the state.
            state.images.insert(
                image_id.to_owned(),
                state::Image {
                    parent_id,
                    last_used_since_epoch: duration,
                },
            );
            Ok(())
        }
        Err(error) => Err(io::Error::new(
            io::ErrorKind::Other,
            format!("Unable to compute the current timestamp: {:?}.", error),
        )),
    }
}

// Construct a polyforest of image nodes that reflects their parent-child relationships.
fn construct_polyforest(
    state: &State,
    image_records: &[ImageRecord],
    image_ids_in_use: &HashSet<String>,
) -> io::Result<HashMap<String, Image>> {
    // Construct a map from image ID to image info.
    let mut image_map = HashMap::new();
    for image_record in image_records {
        image_map.insert(image_record.id.clone(), image_record.clone());
    }

    // Construct the graph. It's a map, just like above, except it has `Image`s rather than
    // `ImageRecord`s. The majority of this code exists just to compute the number of ancestors
    // for each node.
    let mut image_graph = HashMap::new();
    for image_record in image_records {
        // Find the ancestors of the current image, including the current image itself, excluding
        // the root ancestor. The root will be added to the polyforest directly, and later we'll add
        // the other ancestors as well. The first postcondition is that, for every image in this
        // list except the final image, the parent is the subsequent image
        // [tag:image_records_to_add_subsequent_is_parent]. The second postcondition is that the
        // parent of the final image has already been added to the polyforest
        // [tag:image_records_to_add_last_in_polyforest]. Together, these postconditions imply
        // that every image in this list has a parent [tag:image_records_to_add_have_parents].
        let mut image_records_to_add = vec![];
        let mut image_record = image_record.clone();
        loop {
            // Is the image already in the polyforest?
            if image_graph.contains_key(&image_record.id) {
                // The image has already been added.
                break;
            }

            // Does the image have a parent?
            if let Some(parent_id) = &image_record.parent_id {
                // The image has a parent, but does it actually exist?
                if let Some(parent_info) = image_map.get(parent_id) {
                    // It does. Add it to the list of images to add and continue.
                    image_records_to_add.push(image_record.clone());
                    image_record = parent_info.clone();
                    continue;
                }
            }

            // The image is a root because either it has no parent or the parent doesn't exist.
            // Compute the last used date.
            let last_used_since_epoch = state
                .images
                .get(&image_record.id)
                .map_or(image_record.created_since_epoch, |image| {
                    image.last_used_since_epoch
                });

            // Add the image to the polyforest and break.
            image_graph.insert(
                image_record.id.clone(),
                Image {
                    image_record: image_record.clone(),
                    last_used_since_epoch,
                    ancestors: 0,
                },
            );
            break;
        }

        // Add the ancestor images gathered above to the polyforest. We add them in order of
        // ancestor before descendant because we need to ensure the number of ancestors of the
        // parent has already been computed when computing that of the child.
        // [ref:image_records_to_add_subsequent_is_parent]
        while let Some(image_record) = image_records_to_add.pop() {
            // Look up the parent info. The first `unwrap` is safe due to
            // [ref:image_records_to_add_have_parents]. The second `unwrap` is safe due to
            // [ref:image_records_to_add_last_in_polyforest].
            let parent_node = image_graph
                .get(&image_record.parent_id.clone().unwrap())
                .unwrap()
                .clone();

            // Compute the last used date.
            let mut last_used_since_epoch = state
                .images
                .get(&image_record.id)
                .map_or(image_record.created_since_epoch, |image| {
                    image.last_used_since_epoch
                });

            // If the image is in use by a container, update its timestamp.
            if image_ids_in_use.contains(&image_record.id) {
                last_used_since_epoch = max(
                    last_used_since_epoch,
                    match SystemTime::now().duration_since(UNIX_EPOCH) {
                        Ok(duration) => Ok(duration),
                        Err(error) => Err(io::Error::new(
                            io::ErrorKind::Other,
                            format!("Unable to compute the current timestamp: {:?}.", error),
                        )),
                    }?,
                );
            }

            // Add the image.
            image_graph.insert(
                image_record.id.clone(),
                Image {
                    image_record: image_record.clone(),
                    last_used_since_epoch,
                    ancestors: parent_node.ancestors + 1,
                },
            );
        }
    }

    // We need to ensure that each node's last used timestamp is at least as recent as those of all
    // the transitive descendants. We'll do this via a breadth first traversal, starting with the
    // leaves. Since we're working with an acyclic graph, we don't need to keep track of a "visited"
    // set like we usually would for BFS.
    let mut frontier = image_graph.keys().cloned().collect::<HashSet<_>>();
    for image_node in image_graph.values() {
        if let Some(parent_id) = &image_node.image_record.parent_id {
            frontier.remove(parent_id);
        }
    }

    // In each iteration of the outer loop, we traverse the frontier and build up a new frontier
    // for the subsequent iteration.
    while !frontier.is_empty() {
        let mut new_frontier = HashSet::new();

        for image_id in frontier {
            if let Some(image_node) = image_graph.get(&image_id).cloned() {
                if let Some(parent_id) = &image_node.image_record.parent_id {
                    if let Some(parent_node) = image_graph.get_mut(parent_id) {
                        parent_node.last_used_since_epoch = max(
                            parent_node.last_used_since_epoch,
                            image_node.last_used_since_epoch,
                        );
                        new_frontier.insert(parent_node.image_record.id.clone());
                    }
                }
            }
        }

        frontier = new_frontier;
    }

    // If we made it this far, we have a polyforest!
    Ok(image_graph)
}

// The main vacuum logic
fn vacuum(state: &mut State, threshold: &Byte, keep: &Option<RegexSet>) -> io::Result<()> {
    // Find all current images.
    let image_records = list_image_records(state)?;

    // Find all images in use by containers.
    let image_ids_in_use = image_ids_in_use()?;

    // Construct a polyforest of image nodes that reflects their parent-child relationships.
    let image_graph = construct_polyforest(state, &image_records, &image_ids_in_use)?;

    // Sort the images from least recently used to most recently used.
    // Break ties using the number of dependency layers.
    let mut sorted_image_nodes = image_graph.values().collect::<Vec<_>>();
    sorted_image_nodes.sort_by(|x, y| {
        x.last_used_since_epoch
            .cmp(&y.last_used_since_epoch)
            .then(y.ancestors.cmp(&x.ancestors))
    });

    // If the user provided the `--keep` argument, we need to filter out images which match the
    // provided regexes.
    if let Some(regex_set) = keep {
        sorted_image_nodes = sorted_image_nodes
            .into_iter()
            .filter(|image_node| {
                if regex_set.is_match(&format!(
                    "{}:{}",
                    image_node.image_record.repository_tag.repository,
                    image_node.image_record.repository_tag.tag,
                )) {
                    info!(
                        "Ignored image {}{}{} due to the {} flag.",
                        image_node.image_record.repository_tag.repository.code_str(),
                        ":".code_str(),
                        image_node.image_record.repository_tag.tag.code_str(),
                        "--keep".code_str(),
                    );
                    false
                } else {
                    true
                }
            })
            .collect();
    }

    // Check if we're over the threshold.
    let mut deleted_image_ids = HashSet::new();
    let space = space_usage()?;
    if space > *threshold {
        info!(
            "Docker images are currently using {} but the limit is {}.",
            space.get_appropriate_unit(false).to_string().code_str(),
            threshold.get_appropriate_unit(false).to_string().code_str(),
        );

        // Start deleting images, beginning with the least recently used.
        for image_node in sorted_image_nodes {
            // Delete the image.
            if let Err(error) = delete_image(&image_node.image_record.id) {
                // The deletion failed. Just log the error and proceed.
                error!("{}", error);
            } else {
                // Forget about the deleted image.
                deleted_image_ids.insert(image_node.image_record.id.clone());
            }

            // Break if we're within the threshold.
            let new_space = space_usage()?;
            if new_space <= *threshold {
                info!(
                    "Docker images are now using {}, which is within the limit of {}.",
                    new_space.get_appropriate_unit(false).to_string().code_str(),
                    threshold.get_appropriate_unit(false).to_string().code_str(),
                );
                break;
            }
        }
    } else {
        info!(
            "Docker images are using {}, which is within the limit of {}.",
            space.get_appropriate_unit(false).to_string().code_str(),
            threshold.get_appropriate_unit(false).to_string().code_str(),
        );
    }

    // Update the state.
    state.images.clear();
    for image_node in image_graph.values() {
        if !deleted_image_ids.contains(&image_node.image_record.id) {
            state.images.insert(
                image_node.image_record.id.clone(),
                state::Image {
                    parent_id: image_node.image_record.parent_id.clone(),
                    last_used_since_epoch: image_node.last_used_since_epoch,
                },
            );
        }
    }

    Ok(())
}

// Stream Docker events and vacuum when necessary.
pub fn run(settings: &Settings, state: &mut State) -> io::Result<()> {
    // Run the main vacuum logic.
    info!("Performing an initial vacuum on startup\u{2026}");
    vacuum(state, &settings.threshold, &settings.keep)?;
    state::save(&state)?;

    // Spawn `docker events --format '{{json .}}'`.
    let mut child = guard(
        Command::new("docker")
            .args(&["events", "--format", "{{json .}}"])
            .stdout(Stdio::piped())
            .spawn()?,
        |mut child| {
            drop(child.kill());
            drop(child.wait());
        },
    );

    // Buffer the data as we read it line-by-line.
    let reader = BufReader::new(child.stdout.as_mut().map_or_else(
        || {
            Err(io::Error::new(
                io::ErrorKind::Other,
                format!("Unable to read output from {}.", "docker events".code_str()),
            ))
        },
        Ok,
    )?);

    // Handle each incoming event.
    info!("Listening for Docker events\u{2026}");
    for line_option in reader.lines() {
        // Unwrap the line.
        let line = line_option?;
        debug!("Incoming event: {}", line.code_str());

        // Parse the line as an event.
        let event = match serde_json::from_str::<Event>(&line) {
            Ok(event) => {
                debug!("Parsed as: {}", format!("{:?}", event).code_str());
                event
            }
            Err(error) => {
                debug!("Skipping due to: {}", error);
                continue;
            }
        };

        // Get the ID of the image.
        let image_id = image_id(&if event.r#type == "container"
            && (event.action == "create" || event.action == "destroy")
        {
            if let Some(image_name) = event.actor.attributes.image {
                image_name
            } else {
                debug!("Invalid Docker event.");
                continue;
            }
        } else if event.r#type == "image"
            && (event.action == "import"
                || event.action == "load"
                || event.action == "pull"
                || event.action == "push"
                || event.action == "save"
                || event.action == "tag")
        {
            event.id
        } else {
            debug!("Skipping due to irrelevance.");
            continue;
        })?;

        // Inform the user that we're about to vacuum.
        info!("Waking up\u{2026}");

        // Update the timestamp for this image.
        touch_image(state, &image_id, true)?;

        // Run the main vacuum logic.
        vacuum(state, &settings.threshold, &settings.keep)?;

        // Persist the state.
        state::save(&state)?;

        // Inform the user that we're done for now.
        info!("Going back to sleep\u{2026}");
    }

    // The `for` loop above will only terminate if something happened to `docker events`.
    Err(io::Error::new(
        io::ErrorKind::Other,
        format!("{} unexpectedly terminated.", "docker events".code_str()),
    ))
}

#[cfg(test)]
mod tests {
    use super::{construct_polyforest, Image, ImageRecord, RepositoryTag};
    use crate::state::{self, State};
    use std::{
        collections::{HashMap, HashSet},
        io,
        time::Duration,
    };

    #[test]
    fn construct_polyforest_empty() -> io::Result<()> {
        let state = State {
            images: HashMap::new(),
        };

        let image_records = vec![];
        let image_ids_in_use = HashSet::new();
        let image_graph = construct_polyforest(&state, &image_records, &image_ids_in_use)?;

        assert_eq!(0, image_graph.len());

        Ok(())
    }

    #[test]
    fn construct_polyforest_single_image() -> io::Result<()> {
        let image_id = "id-0";

        let mut images = HashMap::new();
        images.insert(
            image_id.to_owned(),
            state::Image {
                parent_id: None,
                last_used_since_epoch: Duration::from_secs(42),
            },
        );

        let state = State { images };

        let image_record = ImageRecord {
            id: image_id.to_owned(),
            parent_id: None,
            created_since_epoch: Duration::from_secs(100),
            repository_tag: RepositoryTag {
                repository: String::from("alpine"),
                tag: String::from("latest"),
            },
        };

        let image_records = vec![image_record.clone()];
        let image_ids_in_use = HashSet::new();
        let image_graph = construct_polyforest(&state, &image_records, &image_ids_in_use)?;

        assert_eq!(1, image_graph.len());

        assert_eq!(
            Some(&Image {
                image_record,
                last_used_since_epoch: Duration::from_secs(42),
                ancestors: 0,
            }),
            image_graph.get(image_id),
        );

        Ok(())
    }

    #[test]
    fn construct_polyforest_single_image_missing_state() -> io::Result<()> {
        let image_id = "id-0";
        let images = HashMap::new();
        let state = State { images };

        let image_record = ImageRecord {
            id: image_id.to_owned(),
            parent_id: None,
            created_since_epoch: Duration::from_secs(100),
            repository_tag: RepositoryTag {
                repository: String::from("alpine"),
                tag: String::from("latest"),
            },
        };

        let image_records = vec![image_record.clone()];
        let image_ids_in_use = HashSet::new();
        let image_graph = construct_polyforest(&state, &image_records, &image_ids_in_use)?;

        assert_eq!(1, image_graph.len());

        assert_eq!(
            Some(&Image {
                image_record,
                last_used_since_epoch: Duration::from_secs(100),
                ancestors: 0,
            }),
            image_graph.get(image_id),
        );

        Ok(())
    }

    #[test]
    fn construct_polyforest_parent_child_increasing_timestamps() -> io::Result<()> {
        let image_id_0 = "id-0";
        let image_id_1 = "id-1";

        let mut images = HashMap::new();
        images.insert(
            image_id_0.to_owned(),
            state::Image {
                parent_id: None,
                last_used_since_epoch: Duration::from_secs(42),
            },
        );
        images.insert(
            image_id_1.to_owned(),
            state::Image {
                parent_id: Some(image_id_0.to_owned()),
                last_used_since_epoch: Duration::from_secs(43),
            },
        );

        let state = State { images };

        let image_record_0 = ImageRecord {
            id: image_id_0.to_owned(),
            parent_id: None,
            created_since_epoch: Duration::from_secs(100),
            repository_tag: RepositoryTag {
                repository: String::from("alpine"),
                tag: String::from("latest"),
            },
        };

        let image_record_1 = ImageRecord {
            id: image_id_1.to_owned(),
            parent_id: Some(image_id_0.to_owned()),
            created_since_epoch: Duration::from_secs(101),
            repository_tag: RepositoryTag {
                repository: String::from("debian"),
                tag: String::from("latest"),
            },
        };

        let image_records = vec![image_record_0.clone(), image_record_1.clone()];
        let image_ids_in_use = HashSet::new();
        let image_graph = construct_polyforest(&state, &image_records, &image_ids_in_use)?;

        assert_eq!(2, image_graph.len());

        assert_eq!(
            Some(&Image {
                image_record: image_record_0,
                last_used_since_epoch: Duration::from_secs(43),
                ancestors: 0,
            }),
            image_graph.get(image_id_0),
        );

        assert_eq!(
            Some(&Image {
                image_record: image_record_1,
                last_used_since_epoch: Duration::from_secs(43),
                ancestors: 1,
            }),
            image_graph.get(image_id_1),
        );

        Ok(())
    }

    #[test]
    fn construct_polyforest_parent_child_decreasing_timestamps() -> io::Result<()> {
        let image_id_0 = "id-0";
        let image_id_1 = "id-1";

        let mut images = HashMap::new();
        images.insert(
            image_id_0.to_owned(),
            state::Image {
                parent_id: None,
                last_used_since_epoch: Duration::from_secs(43),
            },
        );
        images.insert(
            image_id_1.to_owned(),
            state::Image {
                parent_id: Some(image_id_0.to_owned()),
                last_used_since_epoch: Duration::from_secs(42),
            },
        );

        let state = State { images };

        let image_record_0 = ImageRecord {
            id: image_id_0.to_owned(),
            parent_id: None,
            created_since_epoch: Duration::from_secs(100),
            repository_tag: RepositoryTag {
                repository: String::from("alpine"),
                tag: String::from("latest"),
            },
        };

        let image_record_1 = ImageRecord {
            id: image_id_1.to_owned(),
            parent_id: Some(image_id_0.to_owned()),
            created_since_epoch: Duration::from_secs(101),
            repository_tag: RepositoryTag {
                repository: String::from("debian"),
                tag: String::from("latest"),
            },
        };

        let image_records = vec![image_record_0.clone(), image_record_1.clone()];
        let image_ids_in_use = HashSet::new();
        let image_graph = construct_polyforest(&state, &image_records, &image_ids_in_use)?;

        assert_eq!(2, image_graph.len());

        assert_eq!(
            Some(&Image {
                image_record: image_record_0,
                last_used_since_epoch: Duration::from_secs(43),
                ancestors: 0,
            }),
            image_graph.get(image_id_0),
        );

        assert_eq!(
            Some(&Image {
                image_record: image_record_1,
                last_used_since_epoch: Duration::from_secs(42),
                ancestors: 1,
            }),
            image_graph.get(image_id_1),
        );

        Ok(())
    }

    #[test]
    fn construct_polyforest_grandparent_parent_child_increasing_timestamps() -> io::Result<()> {
        let image_id_0 = "id-0";
        let image_id_1 = "id-1";
        let image_id_2 = "id-2";

        let mut images = HashMap::new();
        images.insert(
            image_id_0.to_owned(),
            state::Image {
                parent_id: None,
                last_used_since_epoch: Duration::from_secs(42),
            },
        );
        images.insert(
            image_id_1.to_owned(),
            state::Image {
                parent_id: Some(image_id_0.to_owned()),
                last_used_since_epoch: Duration::from_secs(43),
            },
        );
        images.insert(
            image_id_2.to_owned(),
            state::Image {
                parent_id: Some(image_id_1.to_owned()),
                last_used_since_epoch: Duration::from_secs(44),
            },
        );

        let state = State { images };

        let image_record_0 = ImageRecord {
            id: image_id_0.to_owned(),
            parent_id: None,
            created_since_epoch: Duration::from_secs(100),
            repository_tag: RepositoryTag {
                repository: String::from("alpine"),
                tag: String::from("latest"),
            },
        };

        let image_record_1 = ImageRecord {
            id: image_id_1.to_owned(),
            parent_id: Some(image_id_0.to_owned()),
            created_since_epoch: Duration::from_secs(101),
            repository_tag: RepositoryTag {
                repository: String::from("debian"),
                tag: String::from("latest"),
            },
        };

        let image_record_2 = ImageRecord {
            id: image_id_2.to_owned(),
            parent_id: Some(image_id_1.to_owned()),
            created_since_epoch: Duration::from_secs(102),
            repository_tag: RepositoryTag {
                repository: String::from("ubuntu"),
                tag: String::from("latest"),
            },
        };

        let image_records = vec![
            image_record_0.clone(),
            image_record_1.clone(),
            image_record_2.clone(),
        ];
        let image_ids_in_use = HashSet::new();
        let image_graph = construct_polyforest(&state, &image_records, &image_ids_in_use)?;

        assert_eq!(3, image_graph.len());

        assert_eq!(
            Some(&Image {
                image_record: image_record_0,
                last_used_since_epoch: Duration::from_secs(44),
                ancestors: 0,
            }),
            image_graph.get(image_id_0),
        );

        assert_eq!(
            Some(&Image {
                image_record: image_record_1,
                last_used_since_epoch: Duration::from_secs(44),
                ancestors: 1,
            }),
            image_graph.get(image_id_1),
        );

        assert_eq!(
            Some(&Image {
                image_record: image_record_2,
                last_used_since_epoch: Duration::from_secs(44),
                ancestors: 2,
            }),
            image_graph.get(image_id_2),
        );

        Ok(())
    }

    #[test]
    fn construct_polyforest_grandparent_parent_child_decreasing_timestamps() -> io::Result<()> {
        let image_id_0 = "id-0";
        let image_id_1 = "id-1";
        let image_id_2 = "id-2";

        let mut images = HashMap::new();
        images.insert(
            image_id_0.to_owned(),
            state::Image {
                parent_id: None,
                last_used_since_epoch: Duration::from_secs(44),
            },
        );
        images.insert(
            image_id_1.to_owned(),
            state::Image {
                parent_id: Some(image_id_0.to_owned()),
                last_used_since_epoch: Duration::from_secs(43),
            },
        );
        images.insert(
            image_id_2.to_owned(),
            state::Image {
                parent_id: Some(image_id_1.to_owned()),
                last_used_since_epoch: Duration::from_secs(42),
            },
        );

        let state = State { images };

        let image_record_0 = ImageRecord {
            id: image_id_0.to_owned(),
            parent_id: None,
            created_since_epoch: Duration::from_secs(100),
            repository_tag: RepositoryTag {
                repository: String::from("alpine"),
                tag: String::from("latest"),
            },
        };

        let image_record_1 = ImageRecord {
            id: image_id_1.to_owned(),
            parent_id: Some(image_id_0.to_owned()),
            created_since_epoch: Duration::from_secs(101),
            repository_tag: RepositoryTag {
                repository: String::from("debian"),
                tag: String::from("latest"),
            },
        };

        let image_record_2 = ImageRecord {
            id: image_id_2.to_owned(),
            parent_id: Some(image_id_1.to_owned()),
            created_since_epoch: Duration::from_secs(102),
            repository_tag: RepositoryTag {
                repository: String::from("ubuntu"),
                tag: String::from("latest"),
            },
        };

        let image_records = vec![
            image_record_0.clone(),
            image_record_1.clone(),
            image_record_2.clone(),
        ];
        let image_ids_in_use = HashSet::new();
        let image_graph = construct_polyforest(&state, &image_records, &image_ids_in_use)?;

        assert_eq!(3, image_graph.len());

        assert_eq!(
            Some(&Image {
                image_record: image_record_0,
                last_used_since_epoch: Duration::from_secs(44),
                ancestors: 0,
            }),
            image_graph.get(image_id_0),
        );

        assert_eq!(
            Some(&Image {
                image_record: image_record_1,
                last_used_since_epoch: Duration::from_secs(43),
                ancestors: 1,
            }),
            image_graph.get(image_id_1),
        );

        assert_eq!(
            Some(&Image {
                image_record: image_record_2,
                last_used_since_epoch: Duration::from_secs(42),
                ancestors: 2,
            }),
            image_graph.get(image_id_2),
        );

        Ok(())
    }

    #[test]
    fn construct_polyforest_multiple_children() -> io::Result<()> {
        let image_id_0 = "id-0";
        let image_id_1 = "id-1";
        let image_id_2 = "id-2";

        let mut images = HashMap::new();
        images.insert(
            image_id_0.to_owned(),
            state::Image {
                parent_id: None,
                last_used_since_epoch: Duration::from_secs(43),
            },
        );
        images.insert(
            image_id_1.to_owned(),
            state::Image {
                parent_id: Some(image_id_0.to_owned()),
                last_used_since_epoch: Duration::from_secs(42),
            },
        );
        images.insert(
            image_id_2.to_owned(),
            state::Image {
                parent_id: Some(image_id_0.to_owned()),
                last_used_since_epoch: Duration::from_secs(44),
            },
        );

        let state = State { images };

        let image_record_0 = ImageRecord {
            id: image_id_0.to_owned(),
            parent_id: None,
            created_since_epoch: Duration::from_secs(100),
            repository_tag: RepositoryTag {
                repository: String::from("alpine"),
                tag: String::from("latest"),
            },
        };

        let image_record_1 = ImageRecord {
            id: image_id_1.to_owned(),
            parent_id: Some(image_id_0.to_owned()),
            created_since_epoch: Duration::from_secs(101),
            repository_tag: RepositoryTag {
                repository: String::from("debian"),
                tag: String::from("latest"),
            },
        };

        let image_record_2 = ImageRecord {
            id: image_id_2.to_owned(),
            parent_id: Some(image_id_0.to_owned()),
            created_since_epoch: Duration::from_secs(102),
            repository_tag: RepositoryTag {
                repository: String::from("ubuntu"),
                tag: String::from("latest"),
            },
        };

        let image_records = vec![
            image_record_0.clone(),
            image_record_1.clone(),
            image_record_2.clone(),
        ];
        let image_ids_in_use = HashSet::new();
        let image_graph = construct_polyforest(&state, &image_records, &image_ids_in_use)?;

        assert_eq!(3, image_graph.len());

        assert_eq!(
            Some(&Image {
                image_record: image_record_0,
                last_used_since_epoch: Duration::from_secs(44),
                ancestors: 0,
            }),
            image_graph.get(image_id_0),
        );

        assert_eq!(
            Some(&Image {
                image_record: image_record_1,
                last_used_since_epoch: Duration::from_secs(42),
                ancestors: 1,
            }),
            image_graph.get(image_id_1),
        );

        assert_eq!(
            Some(&Image {
                image_record: image_record_2,
                last_used_since_epoch: Duration::from_secs(44),
                ancestors: 1,
            }),
            image_graph.get(image_id_2),
        );

        Ok(())
    }
}
