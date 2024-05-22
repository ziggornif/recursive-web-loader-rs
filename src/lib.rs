use regex::Regex;
use reqwest::Client;
use scraper::{ElementRef, Html, Selector};
use std::{
    collections::{HashMap, HashSet},
    error::Error,
    future::Future,
    pin::Pin,
    time::Duration,
};

#[derive(Debug)]
pub struct Document {
    pub page_content: String,
    pub metadata: HashMap<String, String>,
}

pub struct RecursiveWebLoaderOptions {
    pub exclude_dirs: Option<Vec<String>>,
    pub max_depth: Option<usize>,
    pub timeout: Option<u64>,
    pub prevent_outside: Option<bool>,
}

impl Default for RecursiveWebLoaderOptions {
    fn default() -> Self {
        Self {
            exclude_dirs: None,
            max_depth: None,
            timeout: None,
            prevent_outside: None,
        }
    }
}

pub struct RecursiveWebLoader {
    url: String,
    exclude_dirs: Vec<String>,
    max_depth: usize,
    timeout: u64,
    prevent_outside: bool,
    client: Client,
}

fn collect_text_not_in_script(element: &ElementRef, text: &mut Vec<String>) {
    for node in element.children() {
        if node.value().is_element() {
            let tag_name = node.value().as_element().unwrap().name();
            if tag_name == "script" {
                continue;
            }
            collect_text_not_in_script(&ElementRef::wrap(node).unwrap(), text);
        } else if node.value().is_text() {
            text.push(node.value().as_text().unwrap().text.to_string());
        }
    }
}

impl RecursiveWebLoader {
    pub fn new(url: String, options: RecursiveWebLoaderOptions) -> Self {
        Self {
            url,
            exclude_dirs: options.exclude_dirs.unwrap_or_default(),
            max_depth: options.max_depth.unwrap_or(2),
            timeout: options.timeout.unwrap_or(10000),
            prevent_outside: options.prevent_outside.unwrap_or(true),
            client: Client::new(),
        }
    }

    async fn fetch_url(&self, url: &str) -> Result<String, Box<dyn Error>> {
        Ok(self
            .client
            .get(url)
            .timeout(Duration::from_millis(self.timeout))
            .send()
            .await?
            .text()
            .await?)
    }

    fn extract_metadata(&self, raw_html: &str, url: &str) -> HashMap<String, String> {
        let mut metadata = HashMap::new();
        metadata.insert("source".to_string(), url.to_string());

        let document = Html::parse_document(raw_html);
        let title_selector = Selector::parse("title").unwrap();
        if let Some(title) = document.select(&title_selector).next() {
            metadata.insert("title".to_string(), title.inner_html());
        }

        let description_selector = Selector::parse("meta[name=description]").unwrap();
        if let Some(description) = document.select(&description_selector).next() {
            if let Some(content) = description.value().attr("content") {
                metadata.insert("description".to_string(), content.to_string());
            }
        }

        let html_selector = Selector::parse("html").unwrap();
        if let Some(html) = document.select(&html_selector).next() {
            if let Some(lang) = html.value().attr("lang") {
                metadata.insert("language".to_string(), lang.to_string());
            }
        }

        metadata
    }

    fn extractor(&self, raw_html: &str) -> String {
        let document = Html::parse_document(raw_html);
        let body_selector = Selector::parse("body").unwrap();

        let mut text = Vec::new();
        for element in document.select(&body_selector) {
            collect_text_not_in_script(&element, &mut text);
        }

        let joined_text = text.join(" ");
        let cleaned_text = joined_text.replace("\n", " ").replace("\t", " ");
        let re = Regex::new(r"\s+").unwrap();
        re.replace_all(&cleaned_text, " ").to_string()
    }

    async fn get_url_as_doc(&self, url: &str) -> Option<Document> {
        match self.fetch_url(url).await {
            Ok(response) => {
                let page_content = self.extractor(&response);
                let metadata = self.extract_metadata(&response, url);
                Some(Document {
                    page_content,
                    metadata,
                })
            }
            Err(_) => None,
        }
    }

    fn get_child_links(&self, html: &str, base_url: &str) -> Vec<String> {
        let document = Html::parse_document(html);
        let selector = Selector::parse("a").unwrap();
        let base_url = reqwest::Url::parse(base_url).unwrap();

        document
            .select(&selector)
            .filter_map(|element| element.value().attr("href"))
            .filter_map(|href| {
                if href.starts_with("http") {
                    Some(href.to_string())
                } else if href.starts_with("//") {
                    Some(format!("{}:{}", base_url.scheme(), href))
                } else {
                    base_url.join(href).ok().map(|url| url.to_string())
                }
            })
            .filter(|link| {
                !self
                    .exclude_dirs
                    .iter()
                    .any(|ex_dir| link.starts_with(ex_dir))
                    && !link.starts_with("javascript:")
                    && !link.starts_with("mailto:")
                    && !link.ends_with(".css")
                    && !link.ends_with(".js")
                    && !link.ends_with(".ico")
                    && !link.ends_with(".png")
                    && !link.ends_with(".jpg")
                    && !link.ends_with(".jpeg")
                    && !link.ends_with(".gif")
                    && !link.ends_with(".svg")
                    && (!self.prevent_outside || link.starts_with(base_url.as_str()))
            })
            .collect()
    }

    fn get_child_urls_recursive<'a>(
        &'a self,
        input_url: &'a str,
        visited: &'a mut HashSet<String>,
        depth: usize,
    ) -> Pin<Box<dyn Future<Output = Vec<Document>> + Send + 'a>> {
        Box::pin(async move {
            if depth >= self.max_depth {
                return vec![];
            }

            let mut url = input_url.to_string();
            if !input_url.ends_with('/') {
                url.push('/');
            }

            if self
                .exclude_dirs
                .iter()
                .any(|ex_dir| url.starts_with(ex_dir))
            {
                return vec![];
            }

            let res = match self.fetch_url(&url).await {
                Ok(res) => res,
                Err(_) => return vec![],
            };

            let child_urls = self.get_child_links(&res, &url);

            let mut results = vec![];

            for child_url in child_urls {
                if visited.contains(&child_url) {
                    continue;
                }
                visited.insert(child_url.clone());

                if let Some(child_doc) = self.get_url_as_doc(&child_url).await {
                    results.push(child_doc);

                    if child_url.ends_with('/') {
                        let mut child_docs = self
                            .get_child_urls_recursive(&child_url, visited, depth + 1)
                            .await;
                        results.append(&mut child_docs);
                    }
                }
            }

            results
        })
    }
    async fn load(&self) -> Vec<Document> {
        let mut docs = vec![];
        if let Some(root_doc) = self.get_url_as_doc(&self.url).await {
            docs.push(root_doc);

            let mut visited = HashSet::new();
            visited.insert(self.url.clone());

            let mut child_docs = self
                .get_child_urls_recursive(&self.url, &mut visited, 0)
                .await;
            docs.append(&mut child_docs);
        }
        docs
    }
}

pub fn add(left: usize, right: usize) -> usize {
    left + right
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn it_works() {
        let result = add(2, 2);
        assert_eq!(result, 4);
    }

    #[tokio::test]
    async fn new_recursive_web_loader() {
        // Request a new server from the pool
        let mut server = mockito::Server::new_async().await;

        // Create a mock on the server
        let mockRoot = server
            .mock("GET", "/")
            .with_status(200)
            .with_header("content-type", "text/plain")
            .with_body("<html><body>Hello World <a href=\"/sub\">foobarbaz</a></body></html>")
            .create();

        let mockSubPath = server
            .mock("GET", "/sub")
            .with_status(200)
            .with_header("content-type", "text/plain")
            .with_body("<html><body>Hi from sub path</body></html>")
            .create();

        let url = server.url();
        let rwl = RecursiveWebLoader::new(url, RecursiveWebLoaderOptions::default());
        let result = rwl.load().await;
        println!("{:?}", result);
        // assert_eq!(result[0], "Hello World");

        mockRoot.assert();
        mockSubPath.assert()
    }
}
