use crate::s3_types::*;
use quick_xml::de::from_str;
use quick_xml::se::to_string;

#[test]
fn test_parse_delete_objects_request() {
    let xml = r#"<Delete>
        <Object>
            <Key>file1</Key>
        </Object>
        <Object>
            <Key>file2</Key>
            <VersionId>123</VersionId>
        </Object>
        <Quiet>true</Quiet>
    </Delete>"#;

    let req: DeleteObjectsRequest = from_str(xml).expect("Should parse");
    assert_eq!(req.objects.len(), 2);
    assert_eq!(req.objects[0].key, "file1");
    assert_eq!(req.objects[1].key, "file2");
    assert_eq!(req.objects[1].version_id, Some("123".to_string()));
    assert!(req.quiet);
}

#[test]
fn test_serialize_delete_objects_result() {
    let result = DeleteObjectsResult {
        deleted: vec![DeletedObject {
            key: "file1".into(),
        }],
        errors: vec![DeleteError {
            key: "file2".into(),
            code: "AccessDenied".into(),
            message: "Access Denied".into(),
        }],
    };

    let xml = to_string(&result).expect("Should serialize");
    assert!(xml.contains("<DeleteResult>"));
    assert!(xml.contains("<Deleted>"));
    assert!(xml.contains("<Key>file1</Key>"));
    assert!(xml.contains("<Error>"));
    assert!(xml.contains("<Code>AccessDenied</Code>"));
}

#[test]
fn test_serialize_copy_object_result() {
    let result = CopyObjectResult {
        last_modified: "2025-01-01T00:00:00Z".into(),
        etag: "\"abc\"".into(),
    };

    let xml = to_string(&result).expect("Should serialize");
    println!("Serialized XML: {}", xml);
    assert!(xml.contains("<CopyObjectResult>"));
    assert!(xml.contains("<LastModified>2025-01-01T00:00:00Z</LastModified>"));
    assert!(xml.contains("<ETag>\"abc\"</ETag>"));
}
